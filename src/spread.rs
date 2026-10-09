//! Premium of venue `a` over venue `b` for one symbol, its in-session rolling median (pair_arb's midline), the
//! deviation from it, and deviation episodes: beyond the band for the persist time, until back within `exit_bp`.

use std::collections::VecDeque;

use crate::{config::SignalCfg, session::Phase, venue::Venue};

#[derive(Clone, Copy, Debug, Default)]
pub struct Book {
    pub bid: f64,
    pub ask: f64,
    pub bid_sz: f64,
    pub ask_sz: f64,
    pub recv_us: u64,
}

impl Book {
    pub fn mid(&self) -> f64 {
        (self.bid + self.ask) / 2.0
    }

    pub fn spread_bp(&self) -> f64 {
        (self.ask - self.bid) / self.mid() * 1e4
    }

    pub fn valid(&self) -> bool {
        self.bid > 0.0 && self.ask > self.bid
    }
}

/// Premium and touch-to-touch edges of `a` against `b`, in bp.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quote {
    pub prem: f64,
    /// buy `a` at its ask, sell `b` at its bid
    pub buy_a: f64,
    /// sell `a` at its bid, buy `b` at its ask
    pub sell_a: f64,
}

impl Quote {
    pub fn of(a: &Book, b: &Book) -> Quote {
        Quote {
            prem: (a.mid() / b.mid() - 1.0) * 1e4,
            buy_a: (b.bid / a.ask - 1.0) * 1e4,
            sell_a: (a.bid / b.ask - 1.0) * 1e4,
        }
    }

    /// Edge of trading toward the midline when the premium deviates in `dir` (+1: `a` rich, so sell `a`).
    pub fn edge_vs_mid(&self, dir: i8, midline: f64) -> f64 {
        if dir > 0 {
            self.sell_a - midline
        } else {
            self.buy_a + midline
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Episode {
    pub sym: String,
    pub pair: String,
    /// +1: `a` rich against the midline, -1: cheap
    pub dir: i8,
    pub phase: Phase,
    pub start_us: u64,
    pub confirm_us: u64,
    pub half_us: Option<u64>,
    pub end_us: Option<u64>,
    pub midline: f64,
    pub confirm_dev: f64,
    pub peak_dev: f64,
    pub peak_edge: f64,
    pub end_dev: Option<f64>,
    pub end_reason: Option<&'static str>,
}

impl Episode {
    pub fn id(&self) -> String {
        format!("{}|{}|{}", self.sym, self.pair, self.start_us)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    pub n: usize,
    pub dev_std: f64,
    pub frac_outside: f64,
    /// Seconds for a deviation to halve under an AR(1) fit of the 1 s deviations, if mean-reverting.
    pub half_life_s: Option<f64>,
    pub ar1: Option<f64>,
}

#[derive(Debug, Default)]
pub struct Step {
    pub midline: Option<f64>,
    pub dev: Option<f64>,
    pub started: Option<Episode>,
    pub ended: Option<Episode>,
}

#[derive(Clone)]
pub struct PairState {
    pub sym: String,
    pub a: Venue,
    pub b: Venue,
    samples: VecDeque<f64>,
    devs: VecDeque<f64>,
    midline: Option<f64>,
    candidate: Option<(u64, i8)>,
    episode: Option<Episode>,
}

pub fn pair_name(a: Venue, b: Venue) -> String {
    format!("{a}/{b}")
}

fn median(v: &VecDeque<f64>) -> f64 {
    let mut s: Vec<f64> = v.iter().copied().collect();
    s.sort_by(f64::total_cmp);
    s[s.len() / 2]
}

impl PairState {
    pub fn new(sym: &str, a: Venue, b: Venue) -> Self {
        Self {
            sym: sym.into(),
            a,
            b,
            samples: VecDeque::new(),
            devs: VecDeque::new(),
            midline: None,
            candidate: None,
            episode: None,
        }
    }

    pub fn pair(&self) -> String {
        pair_name(self.a, self.b)
    }

    pub fn episode(&self) -> Option<&Episode> {
        self.episode.as_ref()
    }

    /// Premiums of the current session from before a restart.
    pub fn seed(&mut self, prems: impl IntoIterator<Item = f64>, cfg: &SignalCfg) {
        for p in prems {
            self.samples.push_back(p);
        }
        while self.samples.len() > cfg.midline_window_s {
            self.samples.pop_front();
        }
        if self.samples.len() >= cfg.midline_min_samples {
            self.midline = Some(median(&self.samples));
        }
    }

    /// Outside the session the midline restarts and an open episode ends.
    pub fn close_session(&mut self, t_us: u64) -> Option<Episode> {
        self.samples.clear();
        self.devs.clear();
        self.midline = None;
        self.candidate = None;
        self.finish(t_us, None, "session_end")
    }

    fn finish(&mut self, t_us: u64, dev: Option<f64>, reason: &'static str) -> Option<Episode> {
        let mut ep = self.episode.take()?;
        ep.end_us = Some(t_us);
        ep.end_dev = dev;
        ep.end_reason = Some(reason);
        Some(ep)
    }

    /// One fresh 1 s sample.
    pub fn step(&mut self, t_us: u64, q: &Quote, phase: Phase, cfg: &SignalCfg) -> Step {
        let mut out = Step::default();
        if !phase.in_session() {
            out.ended = self.close_session(t_us);
            return out;
        }
        self.samples.push_back(q.prem);
        while self.samples.len() > cfg.midline_window_s {
            self.samples.pop_front();
        }
        if self.samples.len() >= cfg.midline_min_samples {
            self.midline = Some(median(&self.samples));
        }
        let Some(mid) = self.midline else {
            return out;
        };
        let dev = q.prem - mid;
        out.midline = Some(mid);
        out.dev = Some(dev);
        self.devs.push_back(dev);
        while self.devs.len() > cfg.stats_window_s {
            self.devs.pop_front();
        }

        if let Some(ep) = self.episode.as_mut() {
            let d = f64::from(ep.dir);
            if d * dev > d * ep.peak_dev {
                ep.peak_dev = dev;
            }
            ep.peak_edge = ep.peak_edge.max(q.edge_vs_mid(ep.dir, mid));
            if ep.half_us.is_none() && d * dev <= d * ep.confirm_dev / 2.0 {
                ep.half_us = Some(t_us);
            }
            let reason = if d * dev <= cfg.exit_bp {
                Some("converged")
            } else if t_us - ep.confirm_us >= cfg.episode_timeout_s * 1_000_000 {
                Some("timeout")
            } else {
                None
            };
            if let Some(reason) = reason {
                out.ended = self.finish(t_us, Some(dev), reason);
                self.candidate = None;
            }
            return out;
        }

        if dev.abs() < cfg.band_bp {
            self.candidate = None;
            return out;
        }
        let dir: i8 = if dev > 0.0 { 1 } else { -1 };
        match self.candidate {
            Some((t0, d0)) if d0 == dir => {
                if t_us - t0 >= cfg.persist_ms * 1_000 {
                    let ep = Episode {
                        sym: self.sym.clone(),
                        pair: self.pair(),
                        dir,
                        phase,
                        start_us: t0,
                        confirm_us: t_us,
                        half_us: None,
                        end_us: None,
                        midline: mid,
                        confirm_dev: dev,
                        peak_dev: dev,
                        peak_edge: q.edge_vs_mid(dir, mid),
                        end_dev: None,
                        end_reason: None,
                    };
                    out.started = Some(ep.clone());
                    self.episode = Some(ep);
                }
            },
            _ => self.candidate = Some((t_us, dir)),
        }
        out
    }

    pub fn stats(&self, cfg: &SignalCfg) -> Option<Stats> {
        let n = self.devs.len();
        if n < 60 {
            return None;
        }
        let mean = self.devs.iter().sum::<f64>() / n as f64;
        let var = self.devs.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / n as f64;
        let outside = self.devs.iter().filter(|d| d.abs() >= cfg.band_bp).count();
        let (mut num, mut den) = (0.0, 0.0);
        for (x0, x1) in self.devs.iter().zip(self.devs.iter().skip(1)) {
            num += (x0 - mean) * (x1 - mean);
            den += (x0 - mean).powi(2);
        }
        let ar1 = (den > 0.0).then(|| num / den);
        Some(Stats {
            n,
            dev_std: var.sqrt(),
            frac_outside: outside as f64 / n as f64,
            half_life_s: ar1
                .filter(|p| *p > 0.0 && *p < 1.0)
                .map(|p| -std::f64::consts::LN_2 / p.ln()),
            ar1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SignalCfg {
        SignalCfg {
            midline_window_s: 100,
            midline_min_samples: 5,
            ..SignalCfg::default()
        }
    }

    /// 20 samples at `prem` ending at t = 19 s, so the midline sits at `prem` through the test.
    fn warm(p: &mut PairState, prem: f64, c: &SignalCfg) {
        for i in 0..20 {
            assert!(p.step(i * S, &q(prem), Phase::Regular, c).started.is_none());
        }
    }

    fn q(prem: f64) -> Quote {
        Quote {
            prem,
            buy_a: -prem - 1.0,
            sell_a: prem - 1.0,
        }
    }

    const S: u64 = 1_000_000;

    #[test]
    fn quotes_price_a_against_b() {
        let a = Book {
            bid: 100.0,
            ask: 100.02,
            ..Book::default()
        };
        let b = Book {
            bid: 99.98,
            ask: 100.0,
            ..Book::default()
        };
        let x = Quote::of(&a, &b);
        assert!((x.prem - 2.0001).abs() < 1e-3);
        assert!((x.sell_a - 0.0).abs() < 1e-9);
        assert!((x.buy_a - (99.98 / 100.02 - 1.0) * 1e4).abs() < 1e-9);
        assert!((x.edge_vs_mid(1, -1.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn an_episode_needs_the_persist_time_and_ends_back_inside_exit() {
        let c = cfg();
        let mut p = PairState::new("SNDK", Venue::HlIo, Venue::HlXyz);
        let ph = Phase::Regular;
        warm(&mut p, 1.0, &c);
        // deviation 4 for 1 s is only a candidate; the 2 s persist confirms it at t=22
        assert!(p.step(20 * S, &q(5.0), ph, &c).started.is_none());
        assert!(p.step(21 * S, &q(5.0), ph, &c).started.is_none());
        let started = p.step(22 * S, &q(5.0), ph, &c).started.unwrap();
        assert_eq!(
            (started.dir, started.start_us, started.confirm_us),
            (1, 20 * S, 22 * S)
        );
        assert_eq!(started.midline, 1.0);

        p.step(23 * S, &q(7.0), ph, &c);
        let half = p.step(24 * S, &q(2.0), ph, &c);
        assert!(half.ended.is_none());
        let end = p.step(25 * S, &q(1.2), ph, &c).ended.unwrap();
        assert_eq!(end.end_reason, Some("converged"));
        assert_eq!(
            (end.peak_dev, end.half_us, end.end_us),
            (6.0, Some(24 * S), Some(25 * S))
        );
        assert!(p.episode().is_none());
    }

    #[test]
    fn a_flip_or_a_gap_resets_the_candidate() {
        let c = cfg();
        let mut p = PairState::new("NBIS", Venue::HlIo, Venue::HlXyz);
        let ph = Phase::Overnight;
        warm(&mut p, 0.0, &c);
        p.step(20 * S, &q(-3.0), ph, &c);
        p.step(21 * S, &q(3.0), ph, &c);
        assert!(p.step(22 * S, &q(0.5), ph, &c).started.is_none());
        p.step(23 * S, &q(-3.0), ph, &c);
        p.step(24 * S, &q(-3.0), ph, &c);
        let ep = p.step(25 * S, &q(-3.0), ph, &c).started.unwrap();
        assert_eq!((ep.dir, ep.start_us), (-1, 23 * S));
    }

    #[test]
    fn episodes_time_out_and_end_with_the_session() {
        let c = SignalCfg {
            episode_timeout_s: 3,
            ..cfg()
        };
        let mut p = PairState::new("EWY", Venue::Lighter, Venue::HlXyz);
        warm(&mut p, 0.0, &c);
        for i in 20..23 {
            p.step(i * S, &q(4.0), Phase::Regular, &c);
        }
        assert!(p.episode().is_some());
        let end = p.step(25 * S, &q(4.0), Phase::Regular, &c).ended.unwrap();
        assert_eq!(end.end_reason, Some("timeout"));

        for i in 26..29 {
            p.step(i * S, &q(4.0), Phase::Regular, &c);
        }
        assert!(p.episode().is_some());
        let end = p.step(30 * S, &q(4.0), Phase::Closed, &c).ended.unwrap();
        assert_eq!(end.end_reason, Some("session_end"));
        assert!(
            p.step(31 * S, &q(4.0), Phase::Regular, &c)
                .midline
                .is_none()
        );
    }

    #[test]
    fn ar1_half_life_of_a_decaying_deviation() {
        let c = SignalCfg {
            midline_window_s: 10_000,
            midline_min_samples: 1,
            stats_window_s: 20_000,
            band_bp: 2.0,
            ..SignalCfg::default()
        };
        let mut p = PairState::new("DRAM", Venue::HlIo, Venue::HlXyz);
        p.seed([0.0; 1], &c);
        // AR(1) deviations x' = 0.9 x + noise: half-life ln 2 / -ln 0.9 = 6.58 s
        let (mut x, mut seed) = (0.0f64, 12345u64);
        for _ in 0..20_000u64 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let noise = ((seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 2.0;
            x = 0.9 * x + noise;
            p.devs.push_back(x);
        }
        let s = p.stats(&c).unwrap();
        assert!((s.half_life_s.unwrap() - 6.58).abs() < 0.5, "{s:?}");
        assert!(s.frac_outside > 0.0 && s.frac_outside < 1.0);
    }
}
