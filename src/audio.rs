//! Traitement audio portable : rééchantillonnage, enveloppe de niveau,
//! WAV en écriture continue (réparable après plantage), FLAC incrémental,
//! décodage des formats courants. Rien ici ne dépend de Windows.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Fréquence des pistes conservées : largement suffisante pour la voix
/// (bande passante 12 kHz), deux fois plus légère que 48 kHz.
pub const STORE_RATE: u32 = 24_000;
/// Fréquence envoyée à ElevenLabs, celle recommandée pour la transcription.
pub const UPLOAD_RATE: u32 = 16_000;
/// Images d'enveloppe par seconde (une toutes les 20 ms).
pub const ENV_FPS: u32 = 50;
const ENV_FLOOR_DB: f32 = -90.0;

// ---------------------------------------------------------------------------
// Conversions de base
// ---------------------------------------------------------------------------
#[inline]
pub fn f32_to_i16(x: f32) -> i16 {
    (x.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

#[inline]
pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Mélange des canaux entrelacés en mono.
pub fn downmix(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels <= 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    let inv = 1.0 / channels as f32;
    out.extend(interleaved.chunks_exact(channels).map(|f| f.iter().sum::<f32>() * inv));
}

// ---------------------------------------------------------------------------
// Rééchantillonnage en flux continu, latence compensée
// ---------------------------------------------------------------------------
pub struct Resampler {
    from: u32,
    to: u32,
    inner: Option<rubato::FftFixedIn<f32>>,
    chunk: usize,
    buf: Vec<f32>,
    skip: usize,
    fed: u64,
    produced: u64,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let chunk = 1024;
        let inner = if from == to {
            None
        } else {
            Some(rubato::FftFixedIn::<f32>::new(from as usize, to as usize, chunk, 2, 1).expect("rééchantillonneur"))
        };
        let skip = inner.as_ref().map(|r| rubato::Resampler::output_delay(r)).unwrap_or(0);
        Resampler { from, to, inner, chunk, buf: Vec::with_capacity(chunk * 2), skip, fed: 0, produced: 0 }
    }

    pub fn rates(&self) -> (u32, u32) {
        (self.from, self.to)
    }

    /// Échantillons de sortie encore retenus (latence + tampon d'entrée).
    pub fn pending_out(&self) -> f64 {
        (self.fed as f64 * self.to as f64 / self.from as f64 - self.produced as f64).max(0.0)
    }

    fn emit(&mut self, data: &[f32], out: &mut Vec<f32>, limit: Option<u64>) {
        let mut d = data;
        if self.skip > 0 {
            let n = self.skip.min(d.len());
            self.skip -= n;
            d = &d[n..];
        }
        if let Some(lim) = limit {
            let room = lim.saturating_sub(self.produced) as usize;
            d = &d[..d.len().min(room)];
        }
        out.extend_from_slice(d);
        self.produced += d.len() as u64;
    }

    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.fed += input.len() as u64;
        let Some(r) = self.inner.as_mut() else {
            out.extend_from_slice(input);
            self.produced += input.len() as u64;
            return;
        };
        let _ = r;
        self.buf.extend_from_slice(input);
        let mut start = 0;
        while self.buf.len() - start >= self.chunk {
            let res = {
                let r = self.inner.as_mut().unwrap();
                rubato::Resampler::process(r, &[&self.buf[start..start + self.chunk]], None)
            };
            if let Ok(v) = res {
                let v = v.into_iter().next().unwrap_or_default();
                self.emit(&v, out, None);
            }
            start += self.chunk;
        }
        self.buf.drain(..start);
    }

    /// Vide le tampon : la sortie totale vaut exactement entrée × rapport.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        if self.inner.is_none() {
            return;
        }
        let target = (self.fed as f64 * self.to as f64 / self.from as f64).round() as u64;
        let rest = std::mem::take(&mut self.buf);
        let mut first = true;
        let mut guard = 0;
        while self.produced < target && guard < 64 {
            guard += 1;
            let res = {
                let r = self.inner.as_mut().unwrap();
                if first {
                    rubato::Resampler::process_partial(r, Some(&[&rest[..]]), None)
                } else {
                    rubato::Resampler::process_partial::<&[f32]>(r, None, None)
                }
            };
            first = false;
            match res {
                Ok(v) => {
                    let v = v.into_iter().next().unwrap_or_default();
                    self.emit(&v, out, Some(target));
                }
                Err(_) => break,
            }
        }
        // Filet de sécurité : longueur exacte quoi qu'il arrive.
        while self.produced < target {
            out.push(0.0);
            self.produced += 1;
        }
    }
}

/// Rééchantillonne un tampon entier (tests, petites conversions).
pub fn resample_all(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    let mut r = Resampler::new(from, to);
    let mut out = Vec::with_capacity(input.len() * to as usize / from.max(1) as usize + 16);
    r.push(input, &mut out);
    r.finish(&mut out);
    out
}

// ---------------------------------------------------------------------------
// Enveloppe : niveau RMS toutes les 20 ms, quantifié sur un octet
// ---------------------------------------------------------------------------
pub fn quantize_db(db: f32) -> u8 {
    (((db.max(ENV_FLOOR_DB).min(0.0) - ENV_FLOOR_DB) / -ENV_FLOOR_DB) * 255.0).round() as u8
}

pub fn env_db(q: u8) -> f32 {
    q as f32 / 255.0 * -ENV_FLOOR_DB + ENV_FLOOR_DB
}

pub struct EnvelopeBuilder {
    frame_len: usize,
    acc: f64,
    n: usize,
    pub frames: Vec<u8>,
    pub peak: f32,
}

impl EnvelopeBuilder {
    pub fn new(rate: u32) -> Self {
        EnvelopeBuilder { frame_len: (rate / ENV_FPS) as usize, acc: 0.0, n: 0, frames: vec![], peak: 0.0 }
    }

    pub fn push(&mut self, samples: &[f32]) {
        for &s in samples {
            self.acc += (s as f64) * (s as f64);
            self.n += 1;
            let a = s.abs();
            if a > self.peak {
                self.peak = a;
            }
            if self.n == self.frame_len {
                self.close_frame();
            }
        }
    }

    fn close_frame(&mut self) {
        let rms = (self.acc / self.n.max(1) as f64).sqrt() as f32;
        let db = if rms > 0.0 { 20.0 * rms.log10() } else { ENV_FLOOR_DB };
        self.frames.push(quantize_db(db));
        self.acc = 0.0;
        self.n = 0;
    }

    pub fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.close_frame();
        }
        self.frames
    }
}

/// Statistiques de parole tirées d'une enveloppe.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Activity {
    /// Bruit de fond estimé (dBFS).
    pub floor_db: f32,
    /// Niveau typique de la parole (dBFS).
    pub speech_db: f32,
    /// Part du temps où quelqu'un parle sur cette piste.
    pub speech_ratio: f32,
}

pub fn activity(env: &[u8]) -> Activity {
    if env.is_empty() {
        return Activity { floor_db: ENV_FLOOR_DB, speech_db: ENV_FLOOR_DB, speech_ratio: 0.0 };
    }
    let mut v: Vec<f32> = env.iter().map(|&q| env_db(q)).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f32| v[((v.len() - 1) as f32 * p) as usize];
    let floor = pct(0.10);
    let thr = speech_threshold(floor);
    let active: Vec<f32> = v.iter().copied().filter(|&d| d >= thr).collect();
    let speech_db = if active.is_empty() { floor } else { active[((active.len() - 1) as f32 * 0.7) as usize] };
    Activity { floor_db: floor, speech_db, speech_ratio: active.len() as f32 / v.len() as f32 }
}

pub fn speech_threshold(floor_db: f32) -> f32 {
    (floor_db + 12.0).max(-55.0)
}

/// Gain qui amène la parole d'une piste autour de -20 dBFS.
pub fn speech_gain_db(a: &Activity) -> f32 {
    if a.speech_ratio < 0.005 {
        return 0.0;
    }
    (-20.0 - a.speech_db).clamp(-6.0, 24.0)
}

/// Le micro capte-t-il les haut-parleurs ? Corrélation des niveaux micro et
/// PC aux instants où le PC émet, avec un décalage toléré de ±200 ms.
/// Renvoie None si le PC parle trop peu pour conclure.
pub fn echo_coupling(mic: &[u8], pc: &[u8]) -> Option<f32> {
    let n = mic.len().min(pc.len());
    if n == 0 {
        return None;
    }
    let pc_act = activity(&pc[..n]);
    let thr = speech_threshold(pc_act.floor_db);
    let idx: Vec<usize> = (10..n.saturating_sub(10)).filter(|&i| env_db(pc[i]) >= thr).collect();
    if idx.len() < 15 * ENV_FPS as usize {
        return None;
    }
    let mut best = f32::MIN;
    for lag in -10i64..=10 {
        let (mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0f64, 0f64, 0f64, 0f64, 0f64);
        for &i in &idx {
            let j = (i as i64 + lag) as usize;
            let x = env_db(pc[i]) as f64;
            let y = env_db(mic[j]) as f64;
            sx += x;
            sy += y;
            sxx += x * x;
            syy += y * y;
            sxy += x * y;
        }
        let k = idx.len() as f64;
        let cov = sxy / k - (sx / k) * (sy / k);
        let vx = sxx / k - (sx / k).powi(2);
        let vy = syy / k - (sy / k).powi(2);
        if vx > 1e-9 && vy > 1e-9 {
            best = best.max((cov / (vx.sqrt() * vy.sqrt())) as f32);
        }
    }
    (best > f32::MIN).then_some(best)
}

// ---------------------------------------------------------------------------
// WAV 16 bits mono en écriture continue
// ---------------------------------------------------------------------------
const WAV_HEADER: usize = 44;

fn wav_header(rate: u32, data_bytes: u32) -> [u8; WAV_HEADER] {
    let mut h = [0u8; WAV_HEADER];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_bytes).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&1u16.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 2).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    h
}

/// L'en-tête est remis à jour à chaque `checkpoint` : après un plantage, le
/// fichier est lisible jusqu'au dernier point, et `repair_wav` récupère le reste.
pub struct WavWriter {
    out: BufWriter<File>,
    rate: u32,
    frames: u64,
    scratch: Vec<u8>,
}

impl WavWriter {
    pub fn create(path: &Path, rate: u32) -> io::Result<Self> {
        let mut f = OpenOptions::new().create(true).truncate(true).write(true).read(true).open(path)?;
        f.write_all(&wav_header(rate, 0))?;
        Ok(WavWriter { out: BufWriter::with_capacity(256 * 1024, f), rate, frames: 0, scratch: vec![] })
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        self.scratch.clear();
        for &s in samples {
            self.scratch.extend_from_slice(&f32_to_i16(s).to_le_bytes());
        }
        self.out.write_all(&self.scratch)?;
        self.frames += samples.len() as u64;
        Ok(())
    }

    pub fn write_silence(&mut self, n: u64) -> io::Result<()> {
        let zeros = [0u8; 8192];
        let mut left = n * 2;
        while left > 0 {
            let k = left.min(zeros.len() as u64) as usize;
            self.out.write_all(&zeros[..k])?;
            left -= k as u64;
        }
        self.frames += n;
        Ok(())
    }

    /// Écrit le tampon sur le disque et met l'en-tête à jour.
    pub fn checkpoint(&mut self, sync: bool) -> io::Result<()> {
        self.out.flush()?;
        let bytes = (self.frames * 2).min(u32::MAX as u64 - 36) as u32;
        let f = self.out.get_mut();
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&wav_header(self.rate, bytes))?;
        f.seek(SeekFrom::End(0))?;
        if sync {
            f.sync_data()?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<u64> {
        self.checkpoint(true)?;
        Ok(self.frames)
    }
}

/// Lit l'en-tête d'un WAV PCM 16 bits : (fréquence, canaux, début des données, taille annoncée).
fn wav_layout(f: &mut File) -> io::Result<(u32, u16, u64, u64)> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let mut head = [0u8; 12];
    f.seek(SeekFrom::Start(0))?;
    f.read_exact(&mut head)?;
    if &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return Err(bad("pas un fichier WAV"));
    }
    let (mut rate, mut channels, mut bits) = (0u32, 0u16, 0u16);
    let mut pos = 12u64;
    loop {
        let mut ch = [0u8; 8];
        f.seek(SeekFrom::Start(pos))?;
        if f.read_exact(&mut ch).is_err() {
            return Err(bad("bloc data introuvable"));
        }
        let size = u32::from_le_bytes(ch[4..8].try_into().unwrap()) as u64;
        if &ch[0..4] == b"fmt " {
            let mut fmt = [0u8; 16];
            f.read_exact(&mut fmt)?;
            channels = u16::from_le_bytes([fmt[2], fmt[3]]);
            rate = u32::from_le_bytes(fmt[4..8].try_into().unwrap());
            bits = u16::from_le_bytes([fmt[14], fmt[15]]);
        } else if &ch[0..4] == b"data" {
            if bits != 16 || rate == 0 || channels == 0 {
                return Err(bad("format WAV non pris en charge"));
            }
            return Ok((rate, channels, pos + 8, size));
        }
        pos += 8 + size + (size & 1);
    }
}

/// Après un plantage : recale l'en-tête sur la taille réelle du fichier.
/// Renvoie (fréquence, nombre d'échantillons récupérés).
pub fn repair_wav(path: &Path) -> io::Result<(u32, u64)> {
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    let (rate, channels, data_at, _) = wav_layout(&mut f)?;
    let len = f.metadata()?.len();
    let block = 2 * channels as u64;
    let data = ((len.saturating_sub(data_at)) / block) * block;
    f.set_len(data_at + data)?;
    let data32 = data.min(u32::MAX as u64 - 36) as u32;
    f.seek(SeekFrom::Start(4))?;
    f.write_all(&((data_at - 8) as u32 + data32).to_le_bytes())?;
    f.seek(SeekFrom::Start(data_at - 4))?;
    f.write_all(&data32.to_le_bytes())?;
    f.sync_all()?;
    Ok((rate, data / block))
}

pub struct WavReader {
    f: io::BufReader<File>,
    pub rate: u32,
    pub channels: u16,
    pub frames: u64,
    left: u64,
    buf: Vec<u8>,
}

impl WavReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut f = File::open(path)?;
        let (rate, channels, data_at, size) = wav_layout(&mut f)?;
        let real = f.metadata()?.len().saturating_sub(data_at);
        let bytes = size.min(real);
        f.seek(SeekFrom::Start(data_at))?;
        let frames = bytes / (2 * channels as u64);
        Ok(WavReader { f: io::BufReader::with_capacity(256 * 1024, f), rate, channels, frames, left: frames, buf: vec![] })
    }

    /// Lit jusqu'à `max` échantillons mono (mélangés si besoin). Vide = fin.
    pub fn read_mono(&mut self, max: usize, out: &mut Vec<f32>) -> io::Result<usize> {
        let n = (max as u64).min(self.left) as usize;
        if n == 0 {
            return Ok(0);
        }
        let ch = self.channels as usize;
        self.buf.resize(n * 2 * ch, 0);
        self.f.read_exact(&mut self.buf)?;
        self.left -= n as u64;
        let inv = 1.0 / (32768.0 * ch as f32);
        for fr in self.buf.chunks_exact(2 * ch) {
            let s: i32 = fr.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as i32).sum();
            out.push(s as f32 * inv);
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// FLAC incrémental (16 bits mono) : rien n'est gardé en mémoire
// ---------------------------------------------------------------------------
pub struct FlacWriter {
    out: BufWriter<File>,
    stream: flacenc::component::Stream,
    config: flacenc::error::Verified<flacenc::config::Encoder>,
    fb: flacenc::source::FrameBuf,
    ctx: flacenc::source::Context,
    pending: Vec<i32>,
    block: usize,
}

impl FlacWriter {
    pub fn create(path: &Path, rate: u32) -> io::Result<Self> {
        use flacenc::component::BitRepr;
        use flacenc::error::Verify;
        let err = |e: String| io::Error::other(e);
        let mut cfg = flacenc::config::Encoder::default();
        cfg.multithread = false;
        let config = cfg.into_verified().map_err(|e| err(format!("{e:?}")))?;
        let block = config.block_size;
        let mut stream = flacenc::component::Stream::new(rate as usize, 1, 16).map_err(|e| err(format!("{e:?}")))?;
        stream.stream_info_mut().set_block_sizes(block, block).map_err(|e| err(format!("{e:?}")))?;
        let mut sink = flacenc::bitsink::ByteSink::new();
        stream.write(&mut sink).map_err(|e| err(format!("{e:?}")))?;
        let mut out = BufWriter::with_capacity(256 * 1024, File::create(path)?);
        out.write_all(sink.as_slice())?;
        Ok(FlacWriter {
            out,
            stream,
            config,
            fb: flacenc::source::FrameBuf::with_size(1, block).map_err(|e| err(format!("{e:?}")))?,
            ctx: flacenc::source::Context::new(16, 1),
            pending: Vec::with_capacity(block * 2),
            block,
        })
    }

    fn encode(&mut self, n: usize) -> io::Result<()> {
        use flacenc::component::BitRepr;
        use flacenc::source::Fill;
        let err = |e: String| io::Error::other(e);
        let frame_samples: Vec<i32> = self.pending.drain(..n).collect();
        (&mut self.fb, &mut self.ctx).fill_interleaved(&frame_samples).map_err(|e| err(format!("{e:?}")))?;
        let num = self.ctx.current_frame_number().unwrap_or(0);
        let frame = flacenc::encode_fixed_size_frame(&self.config, &self.fb, num, self.stream.stream_info())
            .map_err(|e| err(format!("{e:?}")))?;
        let mut sink = flacenc::bitsink::ByteSink::new();
        frame.write(&mut sink).map_err(|e| err(format!("{e:?}")))?;
        self.out.write_all(sink.as_slice())
    }

    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        self.pending.extend(samples.iter().map(|&s| f32_to_i16(s) as i32));
        while self.pending.len() >= self.block {
            self.encode(self.block)?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<u64> {
        use flacenc::component::BitRepr;
        if !self.pending.is_empty() {
            let n = self.pending.len();
            self.encode(n)?;
        }
        let total = self.ctx.total_samples();
        self.stream.stream_info_mut().set_total_samples(total);
        self.stream.stream_info_mut().set_md5_digest(&self.ctx.md5_digest());
        let mut sink = flacenc::bitsink::ByteSink::new();
        self.stream.write(&mut sink).map_err(|e| io::Error::other(format!("{e:?}")))?;
        self.out.flush()?;
        let f = self.out.get_mut();
        f.seek(SeekFrom::Start(0))?;
        f.write_all(sink.as_slice())?;
        f.sync_all()?;
        Ok(total as u64)
    }
}

// ---------------------------------------------------------------------------
// Décodage des fichiers importés (et de nos propres FLAC)
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, serde::Serialize)]
pub struct MediaInfo {
    pub duration: Option<f64>,
    pub rate: Option<u32>,
    pub channels: Option<u32>,
    pub has_video: bool,
    pub codec: Option<String>,
}

fn open_format(path: &Path) -> io::Result<Box<dyn symphonia::core::formats::FormatReader>> {
    use symphonia::core::{formats::FormatOptions, io::MediaSourceStream, meta::MetadataOptions, probe::Hint};
    let file = File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions { enable_gapless: true, ..Default::default() }, &MetadataOptions::default())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(probed.format)
}

pub fn media_info(path: &Path) -> io::Result<MediaInfo> {
    use symphonia::core::codecs::CODEC_TYPE_NULL;
    let fmt = open_format(path)?;
    let tracks = fmt.tracks();
    let audio = tracks.iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL && t.codec_params.sample_rate.is_some());
    let has_video = tracks.len() > 1 && audio.is_some() || tracks.iter().any(|t| t.codec_params.sample_rate.is_none());
    let Some(t) = audio else {
        return Ok(MediaInfo { duration: None, rate: None, channels: None, has_video, codec: None });
    };
    let p = &t.codec_params;
    let duration = match (p.n_frames, p.time_base, p.sample_rate) {
        (Some(n), Some(tb), _) => Some({
            let t = tb.calc_time(n);
            t.seconds as f64 + t.frac
        }),
        (Some(n), None, Some(r)) => Some(n as f64 / r as f64),
        _ => None,
    };
    let codec = symphonia::default::get_codecs().get_codec(p.codec).map(|d| d.short_name.to_string());
    Ok(MediaInfo { duration, rate: p.sample_rate, channels: p.channels.map(|c| c.count() as u32), has_video, codec })
}

/// Décodeur « à la demande » : première piste audio, mono, à la fréquence voulue.
pub struct Decoder {
    fmt: Box<dyn symphonia::core::formats::FormatReader>,
    dec: Box<dyn symphonia::core::codecs::Decoder>,
    track: u32,
    rs: Resampler,
    sbuf: Option<symphonia::core::audio::SampleBuffer<f32>>,
    mono: Vec<f32>,
    errors: u32,
    done: bool,
}

impl Decoder {
    pub fn open(path: &Path, rate: u32) -> io::Result<Self> {
        use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
        let fmt = open_format(path)?;
        let track = fmt
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL && t.codec_params.sample_rate.is_some())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "aucune piste audio"))?
            .clone();
        let dec = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| io::Error::new(io::ErrorKind::Unsupported, e.to_string()))?;
        let src_rate = track.codec_params.sample_rate.unwrap_or(rate);
        Ok(Decoder { fmt, dec, track: track.id, rs: Resampler::new(src_rate, rate), sbuf: None, mono: vec![], errors: 0, done: false })
    }

    /// Ajoute le bloc suivant à `out`. Renvoie false à la fin du fichier.
    pub fn next(&mut self, out: &mut Vec<f32>) -> io::Result<bool> {
        use symphonia::core::audio::SampleBuffer;
        use symphonia::core::errors::Error as SE;
        if self.done {
            return Ok(false);
        }
        loop {
            let pkt = match self.fmt.next_packet() {
                Ok(p) => p,
                Err(SE::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    self.rs.finish(out);
                    self.done = true;
                    return Ok(true);
                }
                Err(SE::ResetRequired) => {
                    self.rs.finish(out);
                    self.done = true;
                    return Ok(true);
                }
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            };
            if pkt.track_id() != self.track {
                continue;
            }
            let decoded = match self.dec.decode(&pkt) {
                Ok(d) => d,
                Err(SE::DecodeError(_)) => {
                    self.errors += 1;
                    if self.errors > 1000 {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "trop d'erreurs de décodage"));
                    }
                    continue;
                }
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            };
            let spec = *decoded.spec();
            let need = decoded.capacity() * spec.channels.count();
            if self.sbuf.as_ref().map(|b| b.capacity() < need).unwrap_or(true) {
                self.sbuf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
            }
            let b = self.sbuf.as_mut().unwrap();
            b.copy_interleaved_ref(decoded);
            self.mono.clear();
            downmix(b.samples(), spec.channels.count(), &mut self.mono);
            self.rs.push(&self.mono, out);
            return Ok(true);
        }
    }
}

/// Décode tout un fichier en flux. `sink` reçoit des blocs ; s'il renvoie
/// false, on arrête. Renvoie le nombre d'échantillons produits.
pub fn decode_mono<F: FnMut(&[f32]) -> bool>(path: &Path, rate: u32, mut sink: F) -> io::Result<u64> {
    let mut d = Decoder::open(path, rate)?;
    let mut buf = Vec::with_capacity(8192);
    let mut total = 0u64;
    loop {
        buf.clear();
        let more = d.next(&mut buf)?;
        total += buf.len() as u64;
        if !buf.is_empty() && !sink(&buf) {
            return Ok(total);
        }
        if !more {
            return Ok(total);
        }
    }
}

/// Lit un fichier mono par blocs d'exactement `n` échantillons (sauf le dernier).
pub struct BlockReader {
    d: Decoder,
    buf: Vec<f32>,
    eof: bool,
}

impl BlockReader {
    pub fn open(path: &Path, rate: u32) -> io::Result<Self> {
        Ok(BlockReader { d: Decoder::open(path, rate)?, buf: vec![], eof: false })
    }

    pub fn read(&mut self, n: usize, out: &mut Vec<f32>) -> io::Result<usize> {
        while self.buf.len() < n && !self.eof {
            if !self.d.next(&mut self.buf)? {
                self.eof = true;
            }
        }
        let k = n.min(self.buf.len());
        out.extend(self.buf.drain(..k));
        Ok(k)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("vb4-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn sine(freq: f32, rate: u32, secs: f32, amp: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    fn zero_crossings(x: &[f32]) -> usize {
        x.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count()
    }

    #[test]
    fn reechantillonnage_longueur_frequence_et_alignement() {
        for (from, to) in [(48_000, 24_000), (44_100, 24_000), (24_000, 16_000), (16_000, 24_000), (24_000, 24_000)] {
            let x = sine(440.0, from, 2.0, 0.5);
            let y = resample_all(&x, from, to);
            assert_eq!(y.len(), (x.len() as f64 * to as f64 / from as f64).round() as usize, "{from}->{to}");
            let zc = zero_crossings(&y[to as usize / 10..to as usize * 19 / 10]);
            assert!((zc as i32 - 792).abs() <= 3, "{from}->{to} : {zc} passages par zéro sur 1,8 s");
            // Une impulsion doit ressortir au même instant (latence compensée).
            let mut imp = vec![0f32; from as usize];
            imp[from as usize / 2] = 1.0;
            let y = resample_all(&imp, from, to);
            let peak = y.iter().enumerate().max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap()).unwrap().0;
            assert!((peak as i64 - to as i64 / 2).abs() <= 2, "{from}->{to} : impulsion à {peak}");
        }
    }

    #[test]
    fn reechantillonnage_par_morceaux_identique() {
        let x = sine(300.0, 48_000, 1.0, 0.4);
        let whole = resample_all(&x, 48_000, 24_000);
        let mut r = Resampler::new(48_000, 24_000);
        let mut parts = vec![];
        for c in x.chunks(333) {
            r.push(c, &mut parts);
        }
        r.finish(&mut parts);
        assert_eq!(whole.len(), parts.len());
        assert!(whole.iter().zip(&parts).all(|(a, b)| (a - b).abs() < 1e-5));
    }

    #[test]
    fn enveloppe_et_activite() {
        let rate = 24_000;
        let mut x = vec![0.0005f32; rate as usize * 10];
        for s in &mut x[rate as usize * 2..rate as usize * 5] {
            *s = 0.1;
        }
        let mut e = EnvelopeBuilder::new(rate);
        e.push(&x);
        let env = e.finish();
        assert_eq!(env.len(), 500);
        let a = activity(&env);
        assert!((a.speech_ratio - 0.3).abs() < 0.02, "{a:?}");
        assert!((a.speech_db + 20.0).abs() < 1.0, "{a:?}");
        assert!((speech_gain_db(&a)).abs() < 1.0);
        assert!((env_db(quantize_db(-33.3)) + 33.3).abs() < 0.2);
    }

    #[test]
    fn wav_continu_puis_reparation_apres_plantage() {
        let d = tmpdir("wav");
        let p = d.join("piste.wav");
        let mut w = WavWriter::create(&p, 24_000).unwrap();
        w.write(&sine(200.0, 24_000, 1.0, 0.5)).unwrap();
        w.checkpoint(false).unwrap();
        w.write_silence(12_000).unwrap();
        w.write(&sine(200.0, 24_000, 0.5, 0.5)).unwrap();
        // Plantage : le tampon part sur le disque, l'en-tête reste ancien.
        w.out.flush().unwrap();
        std::mem::forget(w);
        let r = WavReader::open(&p).unwrap();
        assert_eq!(r.frames, 24_000, "lisible jusqu'au dernier point de contrôle");
        // Octet orphelin en fin de fichier, comme une écriture coupée.
        OpenOptions::new().append(true).open(&p).unwrap().write_all(&[7]).unwrap();
        let (rate, frames) = repair_wav(&p).unwrap();
        assert_eq!((rate, frames), (24_000, 48_000));
        let mut r = WavReader::open(&p).unwrap();
        let mut v = vec![];
        while r.read_mono(10_000, &mut v).unwrap() > 0 {}
        assert_eq!(v.len(), 48_000);
        assert!(v[24_000..36_000].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn flac_incremental_relu_par_symphonia() {
        let d = tmpdir("flac");
        let p = d.join("piste.flac");
        let x = sine(330.0, 24_000, 3.3, 0.6);
        let mut w = FlacWriter::create(&p, 24_000).unwrap();
        for c in x.chunks(1000) {
            w.write(c).unwrap();
        }
        assert_eq!(w.finish().unwrap(), x.len() as u64);
        let info = media_info(&p).unwrap();
        assert_eq!(info.rate, Some(24_000));
        assert!((info.duration.unwrap() - 3.3).abs() < 0.01, "{info:?}");
        let mut back = vec![];
        decode_mono(&p, 24_000, |b| {
            back.extend_from_slice(b);
            true
        })
        .unwrap();
        assert_eq!(back.len(), x.len());
        let err = x.iter().zip(&back).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        assert!(err < 1.0 / 16000.0, "sans perte à la quantification près : {err}");
        assert!(std::fs::metadata(&p).unwrap().len() < (x.len() * 2) as u64, "compressé");
    }

    #[test]
    fn decodage_wav_stereo_vers_mono_16k() {
        let d = tmpdir("dec");
        let p = d.join("st.wav");
        let rate = 44_100u32;
        let l = sine(500.0, rate, 1.0, 0.4);
        let mut data = vec![];
        for s in &l {
            data.extend_from_slice(&f32_to_i16(*s).to_le_bytes());
            data.extend_from_slice(&f32_to_i16(*s).to_le_bytes());
        }
        let mut h = wav_header(rate, data.len() as u32);
        h[22..24].copy_from_slice(&2u16.to_le_bytes());
        h[28..32].copy_from_slice(&(rate * 4).to_le_bytes());
        h[32..34].copy_from_slice(&4u16.to_le_bytes());
        let mut f = File::create(&p).unwrap();
        f.write_all(&h).unwrap();
        f.write_all(&data).unwrap();
        drop(f);
        let info = media_info(&p).unwrap();
        assert_eq!((info.rate, info.channels), (Some(rate), Some(2)));
        let mut n = 0u64;
        let total = decode_mono(&p, UPLOAD_RATE, |b| {
            n += b.len() as u64;
            true
        })
        .unwrap();
        assert_eq!(total, n);
        assert_eq!(total, 16_000);
    }

    fn env_from(levels: &[(f32, usize)]) -> Vec<u8> {
        levels.iter().flat_map(|&(db, n)| std::iter::repeat_n(quantize_db(db), n)).collect()
    }

    #[test]
    fn detection_echo() {
        // PC qui parle par phrases ; micro qui suit (haut-parleurs) ou non (casque).
        let mut pc = vec![];
        for i in 0..60 {
            pc.extend(env_from(&[(-25.0 - (i % 7) as f32 * 2.0, 40), (-70.0, 20)]));
        }
        let echo: Vec<u8> = pc.iter().map(|&q| quantize_db(env_db(q) - 18.0)).collect();
        let mut casque = vec![];
        for i in 0..60 {
            casque.extend(env_from(&[(-75.0, 30), (-22.0 - (i % 5) as f32, 30)]));
        }
        let c_echo = echo_coupling(&echo, &pc).unwrap();
        let c_casque = echo_coupling(&casque, &pc).unwrap();
        assert!(c_echo > 0.8, "{c_echo}");
        assert!(c_casque < 0.4, "{c_casque}");
        assert!(echo_coupling(&casque[..500], &pc[..500]).is_none(), "trop court pour conclure");
    }
}
