//! Metering types, per spec 4.6: per-track/master peak+RMS, true-peak
//! limiting on export, and integrated loudness (LUFS, EBU R128) since
//! deliverables require it. Computing these is M6 (mixer) / M8 (export)
//! work — this is the shape the UI meters and export loudness pass bind to.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeakRms {
    pub peak_dbfs: f32,
    pub rms_dbfs: f32,
}

/// EBU R128 loudness measurement over some window (per-clip or whole
/// sequence, depending on caller).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessMeasurement {
    pub integrated_lufs: f32,
    pub true_peak_dbtp: f32,
    pub loudness_range_lu: f32,
}
