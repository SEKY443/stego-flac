//! Simulated acoustic channel, measuring where each waveform breaks.
//!
//! This crate targets a lossless file channel. The question of whether the same
//! carrier would survive a speaker-to-microphone hop keeps coming up, so rather
//! than answer it from intuition these tests apply the impairments a short
//! acoustic link actually imposes and record what happens.
//!
//! The headline is counter-intuitive: the *slow* M-FSK waveform is enormously
//! robust — it still decodes at 0 dB SNR, where the signal and the noise are
//! equally loud — while the dense OFDM waveform that makes files 60x smaller
//! fails on a one-sample timing shift. Density and robustness are bought with
//! the same currency, and this project spent all of it on density.
//!
//! Note what is *not* simulated: sample-clock offset between the two devices
//! (±20-100 ppm on independent crystals), speaker nonlinearity, and the AGC,
//! noise suppression and echo cancellation a phone applies to microphone input.
//! All three make matters worse, so treat these results as an upper bound.

use audio_modem_core::{from_i16, to_i16, Carrier, OfdmConfig, Plan, Profile};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next() >> 33) as u8).collect()
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Box-Muller, for additive white Gaussian noise.
    fn gauss(&mut self) -> f32 {
        let u1 = self.unit().max(1e-12);
        let u2 = self.unit();
        ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
    }
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

fn awgn(x: &[f32], snr_db: f32, rng: &mut Rng) -> Vec<f32> {
    let sigma = rms(x) / 10f32.powf(snr_db / 20.0);
    x.iter().map(|&s| s + sigma * rng.gauss()).collect()
}

/// Direct path plus one reflection `delay` samples later.
///
/// At 24 kHz a sample is 1.4 cm of path difference, so a 7-sample echo is a
/// surface about 10 cm further away than the direct route — a desk, with the
/// devices sitting on it.
fn multipath(x: &[f32], delay: usize, amplitude: f32) -> Vec<f32> {
    let mut y = x.to_vec();
    for i in delay..x.len() {
        y[i] += amplitude * x[i - delay];
    }
    y
}

/// One-pole high-pass and low-pass, standing in for transducer response.
/// Real speakers and microphones roll off far more steeply, so this is kind.
fn bandlimit(x: &[f32], sample_rate: f32, hp_hz: f32, lp_hz: f32) -> Vec<f32> {
    let dt = 1.0 / sample_rate;
    let a = 1.0 / (std::f32::consts::TAU * hp_hz * dt + 1.0);
    let mut out = Vec::with_capacity(x.len());
    let (mut prev_y, mut prev_x) = (0.0f32, 0.0f32);
    for &s in x {
        let y = a * (prev_y + s - prev_x);
        out.push(y);
        prev_y = y;
        prev_x = s;
    }

    let alpha = std::f32::consts::TAU * lp_hz * dt / (std::f32::consts::TAU * lp_hz * dt + 1.0);
    let mut y = 0.0f32;
    for v in out.iter_mut() {
        y += alpha * (*v - y);
        *v = y;
    }
    out
}

fn survives(modem: &Carrier, payload: &[u8], rx: &[f32]) -> bool {
    let usable = rx.len() - rx.len() % modem.alignment_samples();
    if usable == 0 {
        return false;
    }
    match modem.demodulate(&from_i16(&to_i16(&rx[..usable]))) {
        Ok(bytes) => bytes.len() >= payload.len() && bytes[..payload.len()] == payload[..],
        Err(_) => false,
    }
}

fn ofdm(bits_per_bin: u32) -> Plan {
    Plan::Ofdm(OfdmConfig {
        fft_size: 512,
        base_bin: 8,
        top_bin: 250,
        bits_per_bin,
        ..OfdmConfig::default()
    })
}

// ---------------------------------------------------------------------------

#[test]
fn no_waveform_tolerates_a_timing_offset() {
    // There is no preamble, no correlator, and no timing recovery: the decoder
    // assumes sample 0 of the buffer is sample 0 of symbol 0. A receiver that
    // starts capturing at an arbitrary moment violates that immediately, which
    // is the single hard blocker for any over-the-air use.
    let payload = Rng::new(1).bytes(4000);

    for plan in [Profile::Standard.plan(), Profile::Dense.plan()] {
        let modem = Carrier::new(plan).unwrap();
        let tx = modem.modulate(&payload);

        let mut shifted = vec![0.0f32; 100];
        shifted.extend_from_slice(&tx);
        assert!(
            !survives(&modem, &payload, &shifted),
            "{} unexpectedly tolerated a 100-sample offset; if a sync layer has \
             been added, this test should be rewritten to exercise it",
            plan.describe()
        );
    }
}

#[test]
fn dense_ofdm_is_destroyed_by_a_single_sample_of_delay() {
    // Without a cyclic prefix, any delay spread at all breaks subcarrier
    // orthogonality. The prefix was omitted deliberately -- a bit-exact file has
    // no echo -- and this is exactly the bill for that decision.
    let payload = Rng::new(2).bytes(4000);
    let modem = Carrier::new(Profile::Dense.plan()).unwrap();
    let tx = modem.modulate(&payload);

    assert!(
        !survives(&modem, &payload, &multipath(&tx, 1, 0.5)),
        "dense OFDM survived an echo; has a cyclic prefix been added?"
    );
}

#[test]
fn m_fsk_still_decodes_when_the_noise_is_as_loud_as_the_signal() {
    // 0 dB SNR. Non-coherent M-FSK detection is an argmax over 16 orthogonal
    // bins, and integrating 48 samples buys ~17 dB of processing gain, so the
    // decision survives conditions that annihilate every QAM constellation.
    let payload = Rng::new(3).bytes(2000);
    let modem = Carrier::new(Profile::Standard.plan()).unwrap();
    let tx = modem.modulate(&payload);

    assert!(
        survives(&modem, &payload, &awgn(&tx, 0.0, &mut Rng::new(11))),
        "16-FSK failed at 0 dB SNR"
    );
}

#[test]
fn m_fsk_is_indifferent_to_transducer_response() {
    // Only one tone is on at a time, and the decision compares its bin against
    // silent ones. Attenuating that tone attenuates the winner and the losers
    // alike, so a sloping speaker response cannot change the argmax. QAM has no
    // such immunity: it reads absolute amplitude and phase per subcarrier.
    let payload = Rng::new(4).bytes(2000);
    let modem = Carrier::new(Profile::Standard.plan()).unwrap();
    let tx = modem.modulate(&payload);

    assert!(
        survives(&modem, &payload, &bandlimit(&tx, 24000.0, 500.0, 8000.0)),
        "16-FSK failed through a 500 Hz - 8 kHz channel"
    );
}

#[test]
fn qam_order_trades_throughput_against_every_impairment() {
    let payload = Rng::new(5).bytes(4000);

    // QPSK carries a sixth of the bits but tolerates 20 dB SNR and a -6 dB
    // echo; 4096-QAM carries the most and tolerates neither. Any acoustic
    // configuration would have to sit near the bottom of this table.
    let low = Carrier::new(ofdm(2)).unwrap();
    let tx = low.modulate(&payload);
    assert!(
        survives(&low, &payload, &awgn(&tx, 20.0, &mut Rng::new(21))),
        "QPSK OFDM failed at 20 dB SNR"
    );
    assert!(
        survives(&low, &payload, &multipath(&tx, 7, 0.5)),
        "QPSK OFDM failed with a -6 dB echo 7 samples late"
    );

    let high = Carrier::new(ofdm(12)).unwrap();
    let tx = high.modulate(&payload);
    assert!(
        !survives(&high, &payload, &awgn(&tx, 40.0, &mut Rng::new(22))),
        "4096-QAM survived 40 dB SNR; the constellation may have changed"
    );
}

#[test]
fn frequency_selective_gain_is_what_breaks_ofdm_not_gain_itself() {
    // This isolates the diagnosis. A flat level change of any size decodes
    // perfectly, because the pilots recover it. A *sloping* response of far
    // smaller magnitude does not, because the receiver collapses its pilots into
    // one scalar gain instead of equalising each subcarrier separately.
    //
    // That distinction matters: it means the transducer failure above is an
    // implementation gap with a known fix (per-subcarrier equalisation, which
    // the existing pilots already support), not something fundamental.
    let payload = Rng::new(6).bytes(4000);
    let modem = Carrier::new(ofdm(8)).unwrap();
    let tx = modem.modulate(&payload);

    let flat: Vec<f32> = tx.iter().map(|s| s * 0.05).collect();
    assert!(
        survives(&modem, &payload, &flat),
        "a flat 26 dB attenuation should decode; pilots recover gain"
    );

    assert!(
        !survives(&modem, &payload, &bandlimit(&tx, 24000.0, 200.0, 10000.0)),
        "a sloping response decoded; has per-subcarrier equalisation been added?"
    );
}
