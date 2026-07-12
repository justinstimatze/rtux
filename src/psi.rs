use anyhow::{Context, Result};
use std::fs;

#[derive(Debug, Clone, Copy)]
pub struct PsiLine {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    /// Parsed from PSI for completeness; the ladder decides on avg10.
    #[allow(dead_code)]
    pub total_us: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct PsiReading {
    pub some: PsiLine,
    pub full: Option<PsiLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureLevel {
    Normal,
    Elevated,
    Critical,
}

impl std::fmt::Display for PressureLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PressureLevel::Normal => write!(f, "normal"),
            PressureLevel::Elevated => write!(f, "elevated"),
            PressureLevel::Critical => write!(f, "critical"),
        }
    }
}

/// Read PSI data from a pressure file (/proc/pressure/memory or /proc/pressure/io).
pub fn read_psi(path: &str) -> Result<PsiReading> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading {}", path))?;
    let mut some = None;
    let mut full = None;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("some ") {
            some = Some(parse_psi_line(rest)?);
        } else if let Some(rest) = line.strip_prefix("full ") {
            full = Some(parse_psi_line(rest)?);
        }
    }
    Ok(PsiReading {
        some: some.context("missing 'some' line in PSI data")?,
        full,
    })
}

fn parse_psi_line(s: &str) -> Result<PsiLine> {
    let mut avg10 = 0.0;
    let mut avg60 = 0.0;
    let mut avg300 = 0.0;
    let mut total_us = 0;
    for part in s.split_whitespace() {
        if let Some((key, val)) = part.split_once('=') {
            match key {
                "avg10" => avg10 = val.parse()?,
                "avg60" => avg60 = val.parse()?,
                "avg300" => avg300 = val.parse()?,
                "total" => total_us = val.parse()?,
                _ => {}
            }
        }
    }
    Ok(PsiLine { avg10, avg60, avg300, total_us })
}

/// Classify the current memory pressure level.
pub fn classify_pressure(mem: &PsiReading) -> PressureLevel {
    // Critical: some avg10 > 25% OR full avg10 > 10%
    if mem.some.avg10 > 25.0 {
        return PressureLevel::Critical;
    }
    if let Some(full) = mem.full {
        if full.avg10 > 10.0 {
            return PressureLevel::Critical;
        }
    }
    // Elevated: some avg10 > 5%
    if mem.some.avg10 > 5.0 {
        return PressureLevel::Elevated;
    }
    PressureLevel::Normal
}
