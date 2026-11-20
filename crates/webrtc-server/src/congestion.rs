pub const TWCC_URI: &str =
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Auto,
    Gcc,
    Aimd,
    Off,
}

impl Mode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "gcc" => Ok(Self::Gcc),
            "aimd" => Ok(Self::Aimd),
            "off" => Ok(Self::Off),
            _ => Err("ENZREMOTE_CONGESTION_CONTROL must be auto, gcc, aimd, or off".into()),
        }
    }
}

pub fn create(mode: Mode, ceiling_kbps: u32) -> Result<Option<gst::Element>, String> {
    if matches!(mode, Mode::Aimd | Mode::Off) {
        return Ok(None);
    }
    let ceiling = ceiling_kbps.clamp(128, 100_000) * 1000;
    match gst::ElementFactory::make("rtpgccbwe")
        .property("min-bitrate", 500_000u32.min(ceiling))
        .property("max-bitrate", ceiling)
        .property("estimated-bitrate", 8_000_000u32.min(ceiling))
        .build()
    {
        Ok(element) => Ok(Some(element)),
        Err(_) if mode == Mode::Auto => {
            eprintln!(
                "congestion: rtpgccbwe unavailable; using receiver-feedback AIMD (install gst-plugin-rsrtp for GCC)"
            );
            Ok(None)
        }
        Err(e) => Err(format!(
            "GCC requested but rtpgccbwe could not be created: {e}"
        )),
    }
}

pub fn video_target_kbps(wire_bps: u32, ceiling_kbps: u32) -> u32 {
    let ceiling = ceiling_kbps.clamp(128, 100_000);
    // Reserve 10% for packet overhead/retransmissions and 64 kbps for audio/RTCP.
    (((wire_bps as u64 * 9 / 10).saturating_sub(64_000) / 1000).min(u32::MAX as u64) as u32)
        .clamp(128, ceiling)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bandwidth_budget_reserves_overhead_and_respects_user_ceiling() {
        assert_eq!(video_target_kbps(5_000_000, 5000), 4436);
        assert_eq!(video_target_kbps(30_000_000, 5000), 5000);
        assert_eq!(video_target_kbps(0, 5000), 128);
        assert_eq!(video_target_kbps(u32::MAX, 100_000), 100_000);
        assert_eq!(video_target_kbps(500_000, 128), 128);
    }
}
