//! parse prometheus text-exposition format; extract single metric values.
//! we deliberately use prometheus-parse rather than a hand-rolled scanner so
//! label-set handling stays correct.

use anyhow::{Context, Result, anyhow};
use prometheus_parse::{Scrape, Value};

pub struct Scraped(pub Scrape);

impl Scraped {
    pub fn parse(body: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(body).context("metrics body utf8")?;
        let lines = text.lines().map(|l| Ok::<_, std::io::Error>(l.to_string()));
        Ok(Self(Scrape::parse(lines).context("parse prometheus text exposition")?))
    }

    /// returns the first sample for `name` (any labels). use `gauge_labeled`
    /// for label-specific lookups.
    pub fn gauge(&self, name: &str) -> Result<f64> {
        for sample in &self.0.samples {
            if sample.metric == name {
                return match sample.value {
                    Value::Gauge(v) | Value::Counter(v) | Value::Untyped(v) => Ok(v),
                    Value::Histogram(_) | Value::Summary(_) => {
                        Err(anyhow!("metric {name} is histogram/summary, not gauge"))
                    }
                };
            }
        }
        Err(anyhow!("metric not present: {name}"))
    }

    /// sum over all samples (any labels) for `name`. useful for counters with
    /// many label combinations like `mars_request_total{outcome=..}`.
    pub fn sum(&self, name: &str) -> Result<f64> {
        let mut total = 0.0;
        let mut found = false;
        for sample in &self.0.samples {
            if sample.metric == name {
                found = true;
                match sample.value {
                    Value::Gauge(v) | Value::Counter(v) | Value::Untyped(v) => total += v,
                    Value::Histogram(_) | Value::Summary(_) => continue,
                }
            }
        }
        if !found {
            return Err(anyhow!("metric not present: {name}"));
        }
        Ok(total)
    }
}
