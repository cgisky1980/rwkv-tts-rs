//! 轻量级TTS流水线
//! 复用全局资源，不再每次创建新的模型实例

use crate::{
    dynamic_batch_manager::get_global_dynamic_batch_manager,
    onnx_session_pool::get_global_onnx_manager,
    properties_util,
    rwkv_sampler::{SamplerArgs, TtsBatchRequest},
};
use anyhow::Result;
use ndarray::{Array1, Array2};
use ort::{session::SessionInputValue, value::Value};
use std::path::Path;

/// 轻量级TTS流水线参数
#[derive(Debug, Clone)]
pub struct LightweightTtsPipelineArgs {
    pub text: String,
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: usize,
    pub max_tokens: usize,
    pub age: String,
    pub gender: String,
    pub emotion: String,
    pub pitch: f32,
    pub speed: f32,
    pub zero_shot: bool,
    pub ref_audio_path: String,
    pub prompt_text: String,
    pub output_path: String,
    pub validate: bool,
    pub seed: Option<u64>,
    // 新增：直接传入的音色特征tokens
    pub voice_global_tokens: Option<Vec<i32>>,
    pub voice_semantic_tokens: Option<Vec<i32>>,
}

impl Default for LightweightTtsPipelineArgs {
    fn default() -> Self {
        Self {
            text: String::new(),
            temperature: 1.0,
            top_p: 0.90,
            top_k: 0,
            max_tokens: 8000,
            age: "youth-adult".to_string(),
            gender: "female".to_string(),
            emotion: "NEUTRAL".to_string(),
            pitch: 200.0,
            speed: 4.2,
            zero_shot: false,
            ref_audio_path: String::new(),
            prompt_text: String::new(),
            output_path: String::from("./output"),
            validate: false,
            seed: None,
            voice_global_tokens: None,
            voice_semantic_tokens: None,
        }
    }
}

/// 轻量级TTS流水线，复用全局资源
#[derive(Debug)]
pub struct LightweightTtsPipeline {}

impl LightweightTtsPipeline {
    /// 创建新的轻量级TTS流水线
    pub fn new() -> Self {
        Self {}
    }

    /// 处理文本
    fn process_text(&self, text: &str) -> String {
        text.to_string()
    }

    /// 处理文本（Zero-shot模式）
    /// 注意：Zero-shot模式下结合参考音频的提示文本和用户输入文本
    /// 返回格式为"prompt_text + user_text"的组合，以改善语音合成效果
    pub fn process_text_zero_shot(&self, text: &str, prompt_text: &str) -> String {
        let combined_text = format!("{}{}", prompt_text, text);
        #[cfg(debug_assertions)]
        println!("Zero-shot模式：使用组合文本: {}", combined_text);
        combined_text
    }

    /// 生成TTS属性tokens
    fn generate_property_tokens(&self, args: &LightweightTtsPipelineArgs) -> Vec<i32> {
        // 如果提供了预提取的音色特征tokens或处于zero_shot模式，则不使用传统属性参数
        if (args.voice_global_tokens.is_some() && args.voice_semantic_tokens.is_some())
            || args.zero_shot
        {
            vec![] // 使用预提取音色特征或zero_shot模式时，传统属性参数不起作用
        } else {
            // 对speed和pitch进行分类转换
            let speed_class = properties_util::classify_speed(args.speed);
            // 将字符串年龄转换为数值用于音高分类
            let age_for_pitch = properties_util::age_string_to_number(&args.age);
            let pitch_class =
                properties_util::classify_pitch(args.pitch, &args.gender, age_for_pitch);

            // 直接使用字符串年龄调用convert_standard_properties_to_tokens
            properties_util::convert_standard_properties_to_tokens(
                &speed_class,
                &pitch_class,
                &args.age, // 直接传递字符串年龄
                &args.gender,
                &args.emotion,
            )
        }
    }

    /// 处理参考音频（Zero-shot模式）
    async fn process_reference_audio(&self, ref_audio_path: &str) -> Result<(Vec<i32>, Vec<i32>)> {
        if ref_audio_path.is_empty() || !Path::new(ref_audio_path).exists() {
            return Err(anyhow::anyhow!("参考音频文件不存在: {}", ref_audio_path));
        }

        let onnx_manager = get_global_onnx_manager()?;

        // 加载音频文件
        let audio_data = self.load_audio_file(ref_audio_path).await?;

        // 使用BiCodec Tokenize会话
        let bicodec_session = onnx_manager.acquire_bicodec_tokenize_session().await?;
        let (global_tokens, semantic_tokens) = self
            .tokenize_audio_with_session(&audio_data, bicodec_session)
            .await?;

        Ok((global_tokens, semantic_tokens))
    }

    /// 加载音频文件（支持WAV和MP3格式）
    async fn load_audio_file(&self, audio_path: &str) -> Result<Vec<f32>> {
        use std::path::Path;

        if !Path::new(audio_path).exists() {
            return Err(anyhow::anyhow!("音频文件不存在: {}", audio_path));
        }

        let audio_path = audio_path.to_string();
        let result = tokio::task::spawn_blocking(move || -> Result<Vec<f32>> {
            let path = Path::new(&audio_path);
            let extension = path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
                .to_lowercase();

            let (mut audio, sample_rate, channels) = match extension.as_str() {
                "wav" => {
                    // 使用hound处理WAV文件
                    use hound::WavReader;
                    let mut reader = WavReader::open(&audio_path)?;
                    let spec = reader.spec();

                    let samples: Result<Vec<f32>, _> = reader
                        .samples::<i16>()
                        .map(|s| s.map(|sample| sample as f32 / i16::MAX as f32))
                        .collect();
                    let audio = samples?;

                    (audio, spec.sample_rate, spec.channels as usize)
                }
                "mp3" => {
                    // 使用symphonia处理MP3文件
                    use std::fs::File;
                    use symphonia::core::audio::{AudioBufferRef, Signal};
                    use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_MP3};
                    use symphonia::core::formats::FormatOptions;
                    use symphonia::core::io::MediaSourceStream;
                    use symphonia::core::meta::MetadataOptions;
                    use symphonia::core::probe::Hint;

                    let file = File::open(&audio_path)?;
                    let mss = MediaSourceStream::new(Box::new(file), Default::default());

                    let mut hint = Hint::new();
                    hint.with_extension("mp3");

                    let meta_opts: MetadataOptions = Default::default();
                    let fmt_opts: FormatOptions = Default::default();

                    let probed = symphonia::default::get_probe()
                        .format(&hint, mss, &fmt_opts, &meta_opts)?;

                    let mut format = probed.format;
                    let track = format
                        .tracks()
                        .iter()
                        .find(|t| t.codec_params.codec == CODEC_TYPE_MP3)
                        .ok_or_else(|| anyhow::anyhow!("未找到MP3音轨"))?;

                    let track_id = track.id;
                    let mut decoder = symphonia::default::get_codecs()
                        .make(&track.codec_params, &DecoderOptions { verify: false })?;

                    let mut audio_data = Vec::new();
                    let mut sample_rate = 44100u32;
                    let mut channels = 2usize;

                    while let Ok(packet) = format.next_packet() {
                        if packet.track_id() != track_id {
                            continue;
                        }

                        match decoder.decode(&packet)? {
                            AudioBufferRef::F32(buf) => {
                                sample_rate = buf.spec().rate;
                                channels = buf.spec().channels.count();
                                for &sample in buf.chan(0) {
                                    audio_data.push(sample);
                                }
                                if channels > 1 {
                                    for &sample in buf.chan(1) {
                                        audio_data.push(sample);
                                    }
                                }
                            }
                            AudioBufferRef::S16(buf) => {
                                sample_rate = buf.spec().rate;
                                channels = buf.spec().channels.count();
                                for &sample in buf.chan(0) {
                                    audio_data.push(sample as f32 / i16::MAX as f32);
                                }
                                if channels > 1 {
                                    for &sample in buf.chan(1) {
                                        audio_data.push(sample as f32 / i16::MAX as f32);
                                    }
                                }
                            }
                            _ => return Err(anyhow::anyhow!("不支持的音频格式")),
                        }
                    }

                    (audio_data, sample_rate, channels)
                }
                _ => {
                    return Err(anyhow::anyhow!("不支持的音频格式: {}", extension));
                }
            };

            // 转换为单声道
            if channels > 1 {
                let len = audio.len() / channels;
                let mut mono_audio = Vec::with_capacity(len);
                for i in 0..len {
                    mono_audio.push(audio[i * channels]);
                }
                audio = mono_audio;
            }

            // 重采样到16kHz
            if sample_rate != 16000 {
                let original_len = audio.len();
                let target_len = (original_len as f32 * 16000.0 / sample_rate as f32) as usize;
                let mut resampled = Vec::with_capacity(target_len);
                for i in 0..target_len {
                    let idx = i * original_len / target_len;
                    resampled.push(audio[idx]);
                }
                audio = resampled;
            }

            Ok(audio)
        })
        .await??;

        Ok(result)
    }

    /// 使用ONNX会话进行音频tokenize
    pub async fn tokenize_audio_with_session(
        &self,
        audio_data: &[f32],
        mut session_guard: crate::onnx_session_pool::SessionGuard,
    ) -> Result<(Vec<i32>, Vec<i32>)> {
        // 预先获取wav2vec2会话（异步），随后在阻塞线程中使用
        let onnx_manager = get_global_onnx_manager()?;
        let mut wav2vec2_guard = onnx_manager.acquire_wav2vec2_session().await?;

        let audio_data = audio_data.to_vec();
        let result = tokio::task::spawn_blocking(move || -> Result<(Vec<i32>, Vec<i32>)> {
            // 转换音频数据为ndarray
            let wav = Array1::from(audio_data);

            // 提取参考片段（长度与Ref实现一致）
            let ref_wav = Self::get_ref_clip(&wav);

            // 修复：使用原始wav数据进行wav2vec2特征提取，与C++和Python实现保持一致
            // C++实现中直接使用原始audio进行extract_wav2vec2_features(audio)
            // Python实现中使用原始wav进行extract_wav2vec2_features(wav)
            // 注意：zero_mean_unit_variance_normalize已经在extract_wav2vec2_features内部进行了

            // 应用零均值单位方差归一化预处理 - 与C++和Python实现保持一致
            let normalized_wav =
                crate::ref_audio_utilities::RefAudioUtilities::zero_mean_unit_variance_normalize(
                    wav.to_vec(),
                );
            let feat_input = Array1::from(normalized_wav).insert_axis(ndarray::Axis(0));
            let feat_dyn = feat_input.into_dyn();
            let feat_shape: Vec<i64> = feat_dyn.shape().iter().map(|&d| d as i64).collect();
            let feat_vec = feat_dyn.into_raw_vec();
            let feat_tensor = Value::from_array((feat_shape, feat_vec))?;

            let wav2vec2_outputs = wav2vec2_guard
                .session_mut()
                .run(ort::inputs![SessionInputValue::from(feat_tensor)])?;
            let (feat_out_shape, feat_data) = wav2vec2_outputs[0].try_extract_tensor::<f32>()?;

            if !(feat_out_shape.len() == 3 && feat_out_shape[0] == 1) {
                return Err(anyhow::anyhow!(
                    "Unexpected wav2vec2 output shape: {:?}",
                    feat_out_shape
                ));
            }
            let time_steps = feat_out_shape[1] as usize;
            let feature_dim = feat_out_shape[2] as usize;
            if feature_dim != 1024 {
                return Err(anyhow::anyhow!(
                    "Expected feature dimension 1024, got {}",
                    feature_dim
                ));
            }
            let feat = Array2::from_shape_vec((time_steps, feature_dim), feat_data.to_vec())?;

            // 提取mel频谱图（精确对齐Ref实现）
            // 修复：使用与C++完全一致的参数
            let ref_mel =
                crate::tts_pipeline_fixes::TtsPipelineFixes::extract_mel_spectrogram_consistent(
                    &ref_wav,
                )?;

            // 准备ONNX输入
            // 确保数据是行优先布局（C-order），与C++实现保持一致
            let ref_mel_c_order = if ref_mel.is_standard_layout() {
                ref_mel.clone()
            } else {
                ref_mel.as_standard_layout().to_owned()
            };

            let feat_c_order = if feat.is_standard_layout() {
                feat.clone()
            } else {
                feat.as_standard_layout().to_owned()
            };

            let ref_mel_input = ref_mel_c_order.insert_axis(ndarray::Axis(0));
            let feat_input2 = feat_c_order.insert_axis(ndarray::Axis(0));

            let ref_mel_dyn = ref_mel_input.into_dyn();
            let feat_dyn2 = feat_input2.into_dyn();

            let ref_mel_shape: Vec<i64> = ref_mel_dyn.shape().iter().map(|&d| d as i64).collect();
            let ref_mel_vec: Vec<f32> = ref_mel_dyn.into_raw_vec();
            let ref_mel_tensor = Value::from_array((ref_mel_shape, ref_mel_vec))?;

            let feat_shape2: Vec<i64> = feat_dyn2.shape().iter().map(|&d| d as i64).collect();
            let feat_vec2: Vec<f32> = feat_dyn2.into_raw_vec();
            let feat_tensor2 = Value::from_array((feat_shape2, feat_vec2))?;

            // 运行BiCodec Tokenize ONNX推理
            let outputs = session_guard.session_mut().run(ort::inputs![
                "ref_wav_mel" => SessionInputValue::from(ref_mel_tensor),
                "feat" => SessionInputValue::from(feat_tensor2)
            ])?;

            // 修复输出解析顺序：与Python和C++实现保持一致
            // Python: semantic_tokens = outputs[0], global_tokens = outputs[1]
            // C++: semantic_tokens = output_tensors["semantic_tokens"], global_tokens = output_tensors["global_tokens"]

            // 先尝试按名称解析输出
            let mut semantic_tokens: Vec<i32> = vec![];
            let mut global_tokens: Vec<i32> = vec![];

            // 检查输出名称来确定正确的解析顺序
            for (_name, output) in outputs.iter() {
                // 获取输出的形状信息
                let shape = output.shape();

                // semantic_tokens 通常是形状为 [1, length] 的张量
                // global_tokens 通常是形状为 [1, 1, 32] 的张量
                if shape.len() == 2 && shape[0] == 1 {
                    // 这很可能是 semantic_tokens
                    semantic_tokens = match output.try_extract_tensor::<i64>() {
                        Ok((_s_sem, semantic_tokens_slice)) => {
                            semantic_tokens_slice.iter().map(|&x| x as i32).collect()
                        }
                        Err(_) => {
                            let (_s_sem, semantic_tokens_slice) =
                                output.try_extract_tensor::<i32>()?;
                            semantic_tokens_slice.to_vec()
                        }
                    };
                } else if shape.len() == 3 && shape[0] == 1 && shape[1] == 1 {
                    // 这很可能是 global_tokens
                    global_tokens = match output.try_extract_tensor::<i64>() {
                        Ok((_s_glb, global_tokens_slice)) => {
                            global_tokens_slice.iter().map(|&x| x as i32).collect()
                        }
                        Err(_) => {
                            let (_s_glb, global_tokens_slice) =
                                output.try_extract_tensor::<i32>()?;
                            global_tokens_slice.to_vec()
                        }
                    };
                }
            }

            // 如果按形状没有找到，使用索引方式作为备选
            if semantic_tokens.is_empty() && global_tokens.is_empty() && outputs.len() >= 2 {
                semantic_tokens = match outputs[0].try_extract_tensor::<i64>() {
                    Ok((_s_sem, semantic_tokens_slice)) => {
                        semantic_tokens_slice.iter().map(|&x| x as i32).collect()
                    }
                    Err(_) => {
                        let (_s_sem, semantic_tokens_slice) =
                            outputs[0].try_extract_tensor::<i32>()?;
                        semantic_tokens_slice.to_vec()
                    }
                };

                global_tokens = match outputs[1].try_extract_tensor::<i64>() {
                    Ok((_s_glb, global_tokens_slice)) => {
                        global_tokens_slice.iter().map(|&x| x as i32).collect()
                    }
                    Err(_) => {
                        let (_s_glb, global_tokens_slice) =
                            outputs[1].try_extract_tensor::<i32>()?;
                        global_tokens_slice.to_vec()
                    }
                };
            }

            Ok((global_tokens, semantic_tokens))
        })
        .await??;

        Ok(result)
    }

    /// 创建梅尔滤波器组
    #[allow(dead_code)]
    fn create_mel_filterbank_slaney_with_fmax(
        n_mels: usize,
        n_fft: usize,
        sample_rate: f32,
        fmin: f32,
        fmax: f32,
    ) -> Array2<f32> {
        let n_freqs = n_fft / 2 + 1;
        let mut filterbank = Array2::zeros((n_mels, n_freqs));

        let hz_to_mel = |hz: f32| 2595.0 * (1.0 + hz / 700.0).log10();
        let mel_to_hz = |mel: f32| 700.0 * (10.0_f32.powf(mel / 2595.0) - 1.0);

        let mel_min = hz_to_mel(fmin);
        let mel_max = hz_to_mel(fmax);

        let mel_points: Vec<f32> = (0..=n_mels + 1)
            .map(|i| mel_min + i as f32 * (mel_max - mel_min) / (n_mels + 1) as f32)
            .collect();

        let hz_points: Vec<f32> = mel_points.iter().map(|&mel| mel_to_hz(mel)).collect();
        let bin_points: Vec<f32> = hz_points
            .iter()
            .map(|&hz| hz * n_fft as f32 / sample_rate)
            .collect();

        for m in 1..=n_mels {
            let left = bin_points[m - 1];
            let center = bin_points[m];
            let right = bin_points[m + 1];

            for k in 0..n_freqs {
                let k_f = k as f32;
                if k_f >= left && k_f <= right {
                    if k_f <= center {
                        if center > left {
                            filterbank[[m - 1, k]] = (k_f - left) / (center - left);
                        }
                    } else if right > center {
                        filterbank[[m - 1, k]] = (right - k_f) / (right - center);
                    }
                }
            }

            // Slaney归一化：面积归一化为 2/(fhi-flo)
            let fhi = hz_points[m + 1];
            let flo = hz_points[m - 1];
            let norm_factor = 2.0 / (fhi - flo);
            for k in 0..n_freqs {
                filterbank[[m - 1, k]] *= norm_factor;
            }
        }

        filterbank
    }

    /// 计算功率谱
    #[allow(dead_code)]
    fn compute_power_spectrum(frame: &[f32]) -> Vec<f32> {
        let n_fft = frame.len();
        let n_freqs = n_fft / 2 + 1;
        let mut power_spectrum = vec![0.0f32; n_freqs];

        for (k, power) in power_spectrum.iter_mut().enumerate().take(n_freqs) {
            let mut real = 0.0f32;
            let mut imag = 0.0f32;

            for (n, &sample) in frame.iter().enumerate().take(n_fft) {
                let angle = -2.0 * std::f32::consts::PI * k as f32 * n as f32 / n_fft as f32;
                real += sample * angle.cos();
                imag += sample * angle.sin();
            }

            *power = real * real + imag * imag;
        }

        power_spectrum
    }

    /// 解码音频
    async fn decode_audio(
        &self,
        global_tokens: &[i32],
        semantic_tokens: &[i32],
    ) -> Result<Vec<f32>> {
        let onnx_manager = get_global_onnx_manager()?;

        // 获取BiCodec Detokenize会话
        let detokenize_session = onnx_manager.acquire_bicodec_detokenize_session().await?;

        // 执行解码
        let audio = self
            .detokenize_audio_with_session(global_tokens, semantic_tokens, detokenize_session)
            .await?;

        Ok(audio)
    }

    /// 使用ONNX会话进行音频解码
    async fn detokenize_audio_with_session(
        &self,
        global_tokens: &[i32],
        semantic_tokens: &[i32],
        mut session_guard: crate::onnx_session_pool::SessionGuard,
    ) -> Result<Vec<f32>> {
        // 优化：移除spawn_blocking，直接在异步上下文中执行
        // 直接转换为i64，减少中间步骤和内存分配
        let global_shape: Vec<i64> = [1i64, 1i64, global_tokens.len() as i64].to_vec();
        let global_vec_i64: Vec<i64> = global_tokens.iter().map(|&x| x as i64).collect();
        let global_tensor = Value::from_array((global_shape, global_vec_i64))?;

        let semantic_shape: Vec<i64> = [1i64, semantic_tokens.len() as i64].to_vec();
        let semantic_vec_i64: Vec<i64> = semantic_tokens.iter().map(|&x| x as i64).collect();
        let semantic_tensor = Value::from_array((semantic_shape, semantic_vec_i64))?;

        // 直接运行ONNX推理，避免spawn_blocking的开销
        let outputs = session_guard.session_mut().run(ort::inputs![
            "semantic_tokens" => SessionInputValue::from(semantic_tensor),
            "global_tokens" => SessionInputValue::from(global_tensor)
        ])?;

        let (_shape, audio_slice) = outputs[0].try_extract_tensor::<f32>()?;
        Ok(audio_slice.to_vec())
    }

    /// 生成语音（使用批处理调度器）
    pub async fn generate_speech(&self, args: &LightweightTtsPipelineArgs) -> Result<Vec<f32>> {
        let total_start = std::time::Instant::now();

        #[cfg(debug_assertions)]
        {
            println!("🚀 开始轻量级TTS生成流程");
            println!("  文本: {}", args.text);
            println!("  Zero-shot模式: {}", args.zero_shot);
        }

        // 1. 处理文本
        let text_start = std::time::Instant::now();
        let processed_text = if args.zero_shot {
            self.process_text_zero_shot(&args.text, &args.prompt_text)
        } else {
            self.process_text(&args.text)
        };
        let _text_time = text_start.elapsed();
        #[cfg(debug_assertions)]
        println!(
            "  ⏱️  文本处理耗时: {:.2}ms",
            text_time.as_secs_f64() * 1000.0
        );

        // 2. 处理属性tokens或参考音频
        let ref_start = std::time::Instant::now();
        let (property_tokens, ref_global_tokens, ref_semantic_tokens) =
            // 优先使用直接传入的音色特征tokens
            if let (Some(global_tokens), Some(semantic_tokens)) = (&args.voice_global_tokens, &args.voice_semantic_tokens) {
                #[cfg(debug_assertions)]
                println!("  使用直接传入的音色特征tokens");
                (vec![], Some(global_tokens.clone()), Some(semantic_tokens.clone()))
            } else if args.zero_shot {
                // 在zero-shot模式下，优化为一次性获取所有需要的信息
                // 直接使用传入的音色特征tokens（如果提供了的话）
                if let (Some(global_tokens), Some(semantic_tokens)) = (&args.voice_global_tokens, &args.voice_semantic_tokens) {
                    #[cfg(debug_assertions)]
                    println!("  使用传入的音色特征tokens");
                    (vec![], Some(global_tokens.clone()), Some(semantic_tokens.clone()))
                } else {
                    // 处理参考音频文件
                    let (global, semantic) = self.process_reference_audio(&args.ref_audio_path).await?;
                    (vec![], Some(global), Some(semantic))
                }
            } else {
                let tokens = self.generate_property_tokens(args);
                (tokens, None, None)
            };
        let _ref_time = ref_start.elapsed();
        #[cfg(debug_assertions)]
        {
            if args.voice_global_tokens.is_some() && args.voice_semantic_tokens.is_some() {
                println!(
                    "  ⏱️  音色特征tokens处理耗时: {:.2}ms",
                    ref_time.as_secs_f64() * 1000.0
                );
            } else if args.zero_shot {
                println!(
                    "  ⏱️  参考音频处理耗时: {:.2}ms",
                    ref_time.as_secs_f64() * 1000.0
                );
            } else {
                println!(
                    "  ⏱️  属性tokens生成耗时: {:.2}ms",
                    ref_time.as_secs_f64() * 1000.0
                );
            }
        }

        // 3. 创建采样参数
        let sampler_args = SamplerArgs {
            temperature: args.temperature,
            top_p: args.top_p,
            top_k: args.top_k,
            max_tokens: args.max_tokens,
            seed: args.seed,
            voice_fidelity: 0.8, // 默认音色保真度
            layered_randomness: crate::rwkv_sampler::LayeredRandomnessConfig::default(),
            token_chunk_size: 512, // 使用默认值
        };

        // 4. 创建批处理请求
        let request = TtsBatchRequest {
            text: processed_text,
            property_tokens,
            ref_global_tokens,
            ref_semantic_tokens,
            args: sampler_args,
        };

        // 5. 提交到动态批处理管理器并等待RWKV推理
        let inference_start = std::time::Instant::now();
        let manager = get_global_dynamic_batch_manager()?;
        let (global_tokens, semantic_tokens) = manager
            .generate_tts(
                request.text,
                request.property_tokens,
                request.ref_global_tokens,
                request.ref_semantic_tokens,
                request.args,
            )
            .await?;
        let _inference_time = inference_start.elapsed();
        #[cfg(debug_assertions)]
        println!(
            "  ⏱️  RWKV模型推理耗时: {:.2}ms",
            _inference_time.as_secs_f64() * 1000.0
        );

        #[cfg(debug_assertions)]
        println!(
            "  生成global tokens: {} 个, semantic tokens: {} 个",
            global_tokens.len(),
            semantic_tokens.len()
        );

        // 6. 解码音频
        if global_tokens.is_empty() && semantic_tokens.is_empty() {
            #[cfg(debug_assertions)]
            println!("  未生成任何TTS tokens，返回静音占位");
            return Ok(vec![0.0; 16000]);
        }

        let decode_start = std::time::Instant::now();
        let audio = self.decode_audio(&global_tokens, &semantic_tokens).await?;
        let _decode_time = decode_start.elapsed();
        #[cfg(debug_assertions)]
        println!(
            "  ⏱️  音频解码耗时: {:.2}ms",
            _decode_time.as_secs_f64() * 1000.0
        );

        let total_time = total_start.elapsed();
        let audio_duration = audio.len() as f64 / 16000.0; // 假设16kHz采样率
        let _rtf = total_time.as_secs_f64() / audio_duration;

        #[cfg(debug_assertions)]
        println!(
            "  ⏱️  总耗时: {:.2}ms, 音频时长: {:.2}s, RTF: {:.3}",
            total_time.as_secs_f64() * 1000.0,
            audio_duration,
            _rtf
        );

        // 性能优化建议
        #[cfg(debug_assertions)]
        if _rtf > 0.3 {
            println!("  ⚠️  性能提示: RTF > 0.3，建议优化:");
            if _inference_time.as_secs_f64() > total_time.as_secs_f64() * 0.6 {
                println!(
                    "     - RWKV推理占用{:.1}%时间，考虑使用更激进的量化或更小的模型",
                    _inference_time.as_secs_f64() / total_time.as_secs_f64() * 100.0
                );
            }
            if _decode_time.as_secs_f64() > total_time.as_secs_f64() * 0.3 {
                println!(
                    "     - 音频解码占用{:.1}%时间，考虑优化BiCodec模型或使用GPU加速",
                    _decode_time.as_secs_f64() / total_time.as_secs_f64() * 100.0
                );
            }
            if args.zero_shot && _ref_time.as_secs_f64() > total_time.as_secs_f64() * 0.2 {
                println!(
                    "     - 参考音频处理占用{:.1}%时间，考虑缓存或预处理参考音频",
                    _ref_time.as_secs_f64() / total_time.as_secs_f64() * 100.0
                );
            }
        }

        #[cfg(debug_assertions)]
        println!("  轻量级TTS生成完成，音频长度: {} 样本", audio.len());
        Ok(audio)
    }

    /// 保存音频到文件（支持WAV和MP3格式）
    pub fn save_audio(
        &self,
        audio_samples: &[f32],
        output_path: &str,
        sample_rate: u32,
    ) -> Result<()> {
        use std::path::Path;

        #[cfg(debug_assertions)]
        println!("  保存音频到: {}", output_path);

        let path = Path::new(output_path);
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("wav")
            .to_lowercase();

        match extension.as_str() {
            "mp3" => self.save_audio_mp3(audio_samples, output_path, sample_rate),
            "wav" => self.save_audio_wav(audio_samples, output_path, sample_rate),
            _ => self.save_audio_wav(audio_samples, output_path, sample_rate),
        }
    }

    /// 保存音频到WAV文件
    fn save_audio_wav(
        &self,
        audio_samples: &[f32],
        output_path: &str,
        sample_rate: u32,
    ) -> Result<()> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };

        let mut writer = hound::WavWriter::create(output_path, spec)?;
        for &sample in audio_samples {
            writer.write_sample(sample)?;
        }
        writer.finalize()?;

        #[cfg(debug_assertions)]
        println!("  WAV音频保存完成");
        Ok(())
    }

    /// 保存音频到MP3文件
    fn save_audio_mp3(
        &self,
        audio_samples: &[f32],
        output_path: &str,
        sample_rate: u32,
    ) -> Result<()> {
        use mp3lame_encoder::{Builder, FlushNoGap};
        use std::fs::File;
        use std::io::Write;

        // 转换f32样本到i16
        let i16_samples: Vec<i16> = audio_samples
            .iter()
            .map(|&sample| {
                let clamped = sample.clamp(-1.0, 1.0);
                (clamped * i16::MAX as f32) as i16
            })
            .collect();

        // 配置MP3编码器
        let mut builder = Builder::new().ok_or_else(|| anyhow::anyhow!("创建MP3编码器失败"))?;

        builder
            .set_num_channels(1)
            .map_err(|e| anyhow::anyhow!("设置声道数失败: {}", e))?;

        builder
            .set_sample_rate(sample_rate)
            .map_err(|e| anyhow::anyhow!("设置采样率失败: {}", e))?;

        builder
            .set_brate(mp3lame_encoder::Bitrate::Kbps128)
            .map_err(|e| anyhow::anyhow!("设置比特率失败: {}", e))?;

        builder
            .set_quality(mp3lame_encoder::Quality::Best)
            .map_err(|e| anyhow::anyhow!("设置质量失败: {}", e))?;

        let mut encoder = builder
            .build()
            .map_err(|e| anyhow::anyhow!("构建MP3编码器失败: {}", e))?;

        // 创建输出文件
        let mut output_file =
            File::create(output_path).map_err(|e| anyhow::anyhow!("创建输出文件失败: {}", e))?;

        // 编码音频数据
        use mp3lame_encoder::InterleavedPcm;
        use std::mem::MaybeUninit;

        let mut mp3_buffer: Vec<MaybeUninit<u8>> =
            vec![MaybeUninit::uninit(); i16_samples.len() * 2];
        let pcm_input = InterleavedPcm(&i16_samples);
        let encoded_size = encoder
            .encode(pcm_input, &mut mp3_buffer)
            .map_err(|e| anyhow::anyhow!("MP3编码失败: {}", e))?;

        // 安全地转换MaybeUninit<u8>到u8
        let encoded_data: Vec<u8> = mp3_buffer[..encoded_size]
            .iter()
            .map(|x| unsafe { x.assume_init() })
            .collect();

        output_file
            .write_all(&encoded_data)
            .map_err(|e| anyhow::anyhow!("写入MP3数据失败: {}", e))?;

        // 刷新编码器并写入剩余数据
        let mut flush_buffer: Vec<MaybeUninit<u8>> = vec![MaybeUninit::uninit(); 7200];
        let flush_size = encoder
            .flush::<FlushNoGap>(&mut flush_buffer)
            .map_err(|e| anyhow::anyhow!("刷新MP3编码器失败: {}", e))?;

        if flush_size > 0 {
            let flush_data: Vec<u8> = flush_buffer[..flush_size]
                .iter()
                .map(|x| unsafe { x.assume_init() })
                .collect();

            output_file
                .write_all(&flush_data)
                .map_err(|e| anyhow::anyhow!("写入MP3刷新数据失败: {}", e))?;
        }

        #[cfg(debug_assertions)]
        println!("  MP3音频保存完成");
        Ok(())
    }
}

impl Default for LightweightTtsPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl LightweightTtsPipeline {
    fn get_ref_clip(wav: &Array1<f32>) -> Array1<f32> {
        // 与Ref实现保持一致：长度 = (ref_segment_duration * sample_rate) // latent_hop_length * latent_hop_length
        let sample_rate: u32 = 16000;
        let ref_segment_duration: f32 = 6.0;
        let latent_hop_length: u32 = 320;

        let ref_segment_length = ((ref_segment_duration * sample_rate as f32) as u32
            / latent_hop_length
            * latent_hop_length) as usize;

        let wav_length = wav.len();
        if ref_segment_length > wav_length {
            // 如果音频不足指定长度，重复音频直到达到要求
            let repeat_times = ref_segment_length / wav_length + 1;
            let mut repeated = Vec::with_capacity(wav_length * repeat_times);
            for _ in 0..repeat_times {
                repeated.extend(wav.iter());
            }
            Array1::from(repeated)
                .slice(ndarray::s![..ref_segment_length])
                .to_owned()
        } else {
            // 截取指定长度
            wav.slice(ndarray::s![..ref_segment_length]).to_owned()
        }
    }

    #[allow(dead_code)]
    fn extract_mel_spectrogram_simple(wav: &Array1<f32>) -> Result<Array2<f32>> {
        // 参数与Ref实现一致
        let n_mels: usize = 128;
        let n_fft: usize = 1024;
        let hop_length: usize = 320;
        let win_length: usize = 1024;
        let sample_rate: f32 = 16000.0;

        // center=true 的填充
        let pad_width = n_fft / 2;
        let mut padded_wav = vec![0.0f32; wav.len() + 2 * pad_width];
        for (i, &sample) in wav.iter().enumerate() {
            padded_wav[pad_width + i] = sample;
        }

        let wav_len = padded_wav.len();
        let n_frames = if wav_len <= n_fft {
            1
        } else {
            (wav_len - n_fft) / hop_length + 1
        };

        // Hann窗
        let window: Vec<f32> = if win_length == n_fft {
            (0..n_fft)
                .map(|i| {
                    let angle = 2.0 * std::f32::consts::PI * i as f32 / (n_fft - 1) as f32;
                    0.5 * (1.0 - angle.cos())
                })
                .collect()
        } else {
            let mut window = vec![0.0f32; n_fft];
            let start_pad = (n_fft - win_length) / 2;
            for i in 0..win_length {
                let angle = 2.0 * std::f32::consts::PI * i as f32 / (win_length - 1) as f32;
                window[start_pad + i] = 0.5 * (1.0 - angle.cos());
            }
            window
        };

        // 修复：使用与C++和Python实现一致的参数
        // C++: melSpectrogram(ref_wav_samples, 16000, 1024, 320, 128, 10, 8000, 1.0f, true, false)
        // Python: extract_mel_spectrogram(wav, n_mels=128, n_fft=1024, hop_length=320, win_length=1024)
        // 注意：C++中fmin=10, fmax=8000, power=1.0, center=true, norm=false(slaney)
        let mel_filters =
            Self::create_mel_filterbank_slaney_with_fmax(n_mels, n_fft, sample_rate, 10.0, 8000.0);

        let mut mel_spectrogram = Array2::zeros((n_mels, n_frames));

        for frame_idx in 0..n_frames {
            let start = frame_idx * hop_length;
            let end = (start + n_fft).min(wav_len);

            // 提取帧并应用窗函数
            let mut frame = vec![0.0f32; n_fft];
            for i in 0..(end - start) {
                frame[i] = padded_wav[start + i] * window[i];
            }
            // 零填充剩余部分
            for item in frame.iter_mut().take(n_fft).skip(end - start) {
                *item = 0.0;
            }

            // 计算功率谱（简化版）
            let power_spectrum = Self::compute_power_spectrum(&frame);

            // 应用梅尔滤波器
            for mel_idx in 0..n_mels {
                let mut mel_energy = 0.0f32;
                for freq_idx in 0..power_spectrum.len() {
                    mel_energy += power_spectrum[freq_idx] * mel_filters[[mel_idx, freq_idx]];
                }
                mel_spectrogram[[mel_idx, frame_idx]] = mel_energy;
            }
        }

        Ok(mel_spectrogram)
    }
}

impl LightweightTtsPipeline {
    // 将可能带有统一偏移（如+8196）的codebook token安全归一到模型声码器所需的原始索引空间
    #[allow(dead_code)]
    fn normalize_codebook_offset(tokens: &[i32], offset: i32) -> Vec<i32> {
        if tokens.is_empty() {
            return vec![];
        }
        let min_v = tokens.iter().copied().min().unwrap_or(0);
        if min_v >= offset {
            tokens.iter().map(|&t| t - offset).collect()
        } else {
            tokens.to_vec()
        }
    }
}
