pub fn resize_zeroed(buf: &mut Vec<f32>, len: usize) {
    buf.resize(len, 0.0);
    buf.fill(0.0);
}

pub unsafe fn convert_to_f32_mono_into(
    buffer: *const u8,
    frames: usize,
    channels: usize,
    bits: u16,
    out: &mut Vec<f32>,
) {
    resize_zeroed(out, frames);

    match bits {
        32 => {
            let data =
                unsafe { std::slice::from_raw_parts(buffer as *const f32, frames * channels) };
            for (i, frame) in data.chunks(channels).enumerate() {
                let sum: f32 = frame.iter().sum();
                out[i] = sum / channels as f32;
            }
        }
        16 => {
            let data =
                unsafe { std::slice::from_raw_parts(buffer as *const i16, frames * channels) };
            for (i, frame) in data.chunks(channels).enumerate() {
                let sum: f32 = frame.iter().map(|&s| s as f32 / 32768.0).sum();
                out[i] = sum / channels as f32;
            }
        }
        _ => {}
    }
}

pub fn resample_into(input: &[f32], from_rate: usize, to_rate: usize, out: &mut Vec<f32>) {
    if from_rate == to_rate || input.is_empty() {
        out.resize(input.len(), 0.0);
        out.copy_from_slice(input);
        return;
    }

    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = ((input.len() as f64) / ratio).ceil() as usize;
    out.resize(out_len, 0.0);

    for (i, sample) in out.iter_mut().enumerate() {
        let src_idx = i as f64 * ratio;
        let idx = src_idx as usize;
        let frac = (src_idx - idx as f64) as f32;
        let s0 = input[idx.min(input.len() - 1)];
        let s1 = input[(idx + 1).min(input.len() - 1)];
        *sample = s0 + frac * (s1 - s0);
    }
}
