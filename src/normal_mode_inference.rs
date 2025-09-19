use anyhow::Result;
use rand::rngs::StdRng;
use rand::SeedableRng;
use tracing::warn;
use web_rwkv::runtime::infer::{RnnInput, RnnInputBatch, RnnOption};

use crate::shared_runtime::TtsInferContext;

/// 执行普通模式推理
pub async fn execute_normal_inference(
    infer_context: TtsInferContext,
    text_tokens: Vec<i32>,
    property_tokens: Vec<i32>,
    rng: rand::rngs::StdRng,
    request: &crate::rwkv_sampler::TtsBatchRequest,
) -> Result<(Vec<i32>, Vec<i32>)> {
    let request_id = &infer_context.request_id;
    // 开始普通模式推理

    // 获取采样参数
    let sampler_args = &request.args;

    // Acquire runtime semaphore for the entire inference to ensure isolation
    let _runtime_permit = infer_context
        .runtime_semaphore
        .acquire()
        .await
        .map_err(|e| anyhow::anyhow!("无法获取运行时信号量: {}", e))?;

    // 已获取信号量许可，开始推理

    // 获取runtime
    let runtime = &infer_context.runtime;
    let state = &infer_context.state;

    // 构建输入序列：属性tokens + TTS_TAG_2 + 文本tokens + TTS_TAG_0
    let mut input_tokens: Vec<i32> = Vec::new();
    input_tokens.extend_from_slice(&property_tokens);
    input_tokens.push(crate::rwkv_sampler::TTS_TAG_2);
    input_tokens.extend_from_slice(&text_tokens);
    input_tokens.push(crate::rwkv_sampler::TTS_TAG_0);

    // 构建完整输入序列

    // === Prefill 阶段 ===
    let input_tokens_u32: Vec<u32> = input_tokens.iter().map(|&t| t as u32).collect();
    let token_chunk_size = infer_context.options.token_chunk_size;

    // Prefill阶段 - 初始化独立状态

    // 创建独立的推理上下文
    let batch = RnnInputBatch::new(input_tokens_u32.clone(), RnnOption::Last);
    let mut inference = RnnInput::new(vec![batch], token_chunk_size);

    // 为批处理槽位0加载初始状态，确保状态隔离
    {
        let state_guard = state.lock().await;
        let initial_state = state_guard.init();
        state_guard.load(initial_state, 0)?;
        // 已为批处理槽位0加载初始状态
    }

    // 消化输入直到产生输出
    let last_logits: Vec<f32> = loop {
        let (remaining_input, output) = runtime.infer(inference.clone()).await?;
        inference = remaining_input;
        if !output.is_empty() && output[0].0.size() > 0 {
            break output[0].0.clone().to_vec();
        }
    };

    // === Global 阶段 ===
    let mut global_tokens: Vec<i32> = Vec::new();
    let mut semantic_tokens: Vec<i32> = Vec::new();

    // 普通模式进行正常的生成流程（不使用预提取特征）
    // 从推理上下文获取采样参数
    let mut args_global = crate::rwkv_sampler::SamplerArgs {
        temperature: infer_context.options.temperature,
        top_k: if infer_context.options.top_k == 0 {
            20
        } else {
            infer_context.options.top_k
        },
        top_p: infer_context.options.top_p,
        seed: infer_context.options.seed,
        max_tokens: 32, // Global阶段固定32个tokens
        voice_fidelity: infer_context.options.voice_fidelity,
        layered_randomness: infer_context.options.layered_randomness.clone(),
        token_chunk_size: infer_context.options.token_chunk_size,
    };

    let args_semantic = crate::rwkv_sampler::SamplerArgs {
        temperature: 1.0, // Semantic阶段使用固定参数
        top_p: 0.95,
        top_k: 80,
        seed: infer_context.options.seed,
        max_tokens: 2048,
        voice_fidelity: infer_context.options.voice_fidelity,
        layered_randomness: infer_context.options.layered_randomness.clone(),
        token_chunk_size: infer_context.options.token_chunk_size,
    };

    // 简化采样，移除优化组件

    // 创建独立的RNG用于不同阶段
    let mut global_rng = if args_global.layered_randomness.use_independent_seeds {
        if let Some(seed) = args_global.seed {
            // 用户提供了seed，使用确定性采样
            Some(StdRng::seed_from_u64(seed.wrapping_add(
                args_global.layered_randomness.global_seed_offset,
            )))
        } else {
            // 没有seed，创建随机RNG
            Some(StdRng::from_entropy())
        }
    } else {
        Some(rng.clone())
    };

    let mut semantic_rng = if args_semantic.layered_randomness.use_independent_seeds {
        if let Some(seed) = args_semantic.seed {
            // 用户提供了seed，使用确定性采样
            Some(StdRng::seed_from_u64(seed.wrapping_add(
                args_semantic.layered_randomness.semantic_seed_offset,
            )))
        } else {
            // 没有seed，创建随机RNG
            Some(StdRng::from_entropy())
        }
    } else {
        Some(rng.clone())
    };

    // RNG状态初始化

    // 应用音色保真度调整
    let global_fidelity_factor = sampler_args.voice_fidelity;
    let global_randomness_factor = sampler_args.layered_randomness.global_randomness;
    let global_conservative_factor = global_fidelity_factor * (1.0 - global_randomness_factor);

    // Global阶段采用更保守的参数调整
    args_global.temperature *=
        (0.3_f32 + 0.7_f32 * (1.0_f32 - global_conservative_factor)).max(0.1_f32);
    args_global.top_p =
        (args_global.top_p * (0.8_f32 + 0.2_f32 * global_conservative_factor)).max(0.2_f32);
    args_global.top_k = ((args_global.top_k as f32)
        * (0.9_f32 + 0.1_f32 * global_conservative_factor))
        .max(5.0_f32) as usize;

    // Semantic阶段使用固定参数

    // 生成32个global tokens
    let global_tokens_size: usize = 32;

    for i in 0..global_tokens_size {
        let logits: Vec<f32> = if i == 0 {
            last_logits.clone()
        } else {
            // 继续推理获取logits - 使用现有inference上下文
            loop {
                let (next_inference, output) = runtime.infer(inference.clone()).await?;
                inference = next_inference;
                if output[0].0.size() > 0 {
                    break output[0].0.clone().to_vec();
                }
            }
        };

        // 仅在[0..4096)范围内采样
        let vocab_global = if logits.len() < 4096 {
            logits.len()
        } else {
            4096
        };

        // 直接使用原始logits，不进行增强处理
        let sampling_logits = logits[..vocab_global].to_vec();

        // 使用基本采样
        let next_id = crate::rwkv_sampler::sample_logits_impl(
            &sampling_logits,
            &args_global,
            None, // forbid_token
            &mut global_rng,
        );

        // 安全转换：确保token在有效范围内
        if next_id > i32::MAX as usize {
            warn!(
                "🚨 [{}] Global token {} 超出i32范围，跳过此token",
                request_id, next_id
            );
            continue;
        }

        // 额外检查：确保token在global范围内 [0..4096)
        if next_id >= 4096 {
            warn!(
                "🚨 [{}] Global token {} 超出范围[0..4096)，跳过此token",
                request_id, next_id
            );
            continue;
        }

        global_tokens.push(next_id as i32);

        // 反馈到模型：直接使用原始ID（与C++代码一致）
        inference.batches[0].push(next_id as u32);

        // Global token生成
    }

    // Global tokens生成完成

    // === 切换到 Semantic 阶段 ===
    inference.batches[0].push(crate::rwkv_sampler::TTS_TAG_1 as u32);
    // 切换到Semantic阶段

    // 让标签生效，直到产生输出，并保留logits供首步使用
    let last_sem_logits: Vec<f32> = loop {
        let (next_inference, output) = runtime.infer(inference).await?;
        inference = next_inference;
        if output[0].0.size() > 0 {
            break output[0].0.clone().to_vec();
        }
    };

    // 语义阶段：限制最大生成步数为2048
    let semantic_limit: usize = usize::min(request.args.max_tokens, 2048);
    // 开始生成semantic tokens

    for i in 0..semantic_limit {
        let logits: Vec<f32> = if i == 0 {
            last_sem_logits.clone()
        } else {
            loop {
                let (next_inference, output) = runtime.infer(inference.clone()).await?;
                inference = next_inference;
                if output[0].0.size() > 0 {
                    break output[0].0.clone().to_vec();
                }
            }
        };

        // 语义阶段仅采样 [0..8192]（包含EOS），屏蔽TTS_TAG_*与其它域
        let mut logits_masked = logits.clone();
        // 修复：不屏蔽EOS token，只屏蔽大于EOS token的部分
        for (j, v) in logits_masked.iter_mut().enumerate() {
            if j > crate::rwkv_sampler::TTS_EOS_TOKEN as usize {
                *v = f32::NEG_INFINITY;
            }
        }
        // 屏蔽TTS_TAG tokens，但保留EOS token
        for tag in [
            crate::rwkv_sampler::TTS_TAG_0,
            crate::rwkv_sampler::TTS_TAG_1,
            crate::rwkv_sampler::TTS_TAG_2,
        ] {
            let idx = tag as usize;
            if idx < logits_masked.len() {
                logits_masked[idx] = f32::NEG_INFINITY;
            }
        }

        // 注意：不屏蔽EOS token，让它能够被正常采样以终止生成

        // EOS token logits检查
        let _eos_logit = if (crate::rwkv_sampler::TTS_EOS_TOKEN as usize) < logits_masked.len() {
            logits_masked[crate::rwkv_sampler::TTS_EOS_TOKEN as usize]
        } else {
            f32::NEG_INFINITY
        };

        // 使用基本采样
        let next_id = crate::rwkv_sampler::sample_logits_impl(
            &logits_masked,
            &args_semantic,
            None, // forbid_token
            &mut semantic_rng,
        );

        // 检查是否遇到EOS token（必须在范围检查之前）
        if next_id == crate::rwkv_sampler::TTS_EOS_TOKEN as usize {
            // 遇到EOS token，停止生成
            break;
        }

        // 额外检查：确保token在semantic范围内 [0..8192)（修复：应该是>8192而不是>=8192）
        if next_id > crate::rwkv_sampler::TTS_EOS_TOKEN as usize {
            warn!(
                "🚨 [{}] Token {} 超出semantic范围[0..8192]，跳过此token",
                request_id, next_id
            );
            continue;
        }

        semantic_tokens.push(next_id as i32);

        // 反馈到模型：语义阶段直接使用原始token（不加偏移）
        inference.batches[0].push(next_id as u32);
    }

    Ok((global_tokens, semantic_tokens))
}
