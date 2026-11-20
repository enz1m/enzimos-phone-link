use gst::glib;
use gst::glib::subclass::prelude::*;
use std::sync::atomic::{AtomicU32, Ordering};

pub const URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DelayRange {
    minimum: u16,
    maximum: u16,
}

impl DelayRange {
    pub fn parse(value: &str) -> Result<Option<Self>, String> {
        if value == "adaptive" {
            return Ok(None);
        }
        if value == "interactive" || value == "balanced" {
            return Ok(Some(Self {
                minimum: 0,
                maximum: if value == "balanced" { 120 } else { 0 },
            }));
        }
        let (minimum, maximum) = value.split_once(':').ok_or(
            "Playout delay must be balanced, interactive, adaptive, or MIN:MAX milliseconds",
        )?;
        let minimum = minimum
            .parse::<u16>()
            .map_err(|_| "Invalid minimum playout delay")?;
        let maximum = maximum
            .parse::<u16>()
            .map_err(|_| "Invalid maximum playout delay")?;
        if minimum > maximum || maximum > 40950 || minimum % 10 != 0 || maximum % 10 != 0 {
            return Err("Playout delays must be ordered 0..40950 ms in 10 ms steps".into());
        }
        Ok(Some(Self { minimum, maximum }))
    }

    pub fn bytes(self) -> [u8; 3] {
        let packed = (u32::from(self.minimum / 10) << 12) | u32::from(self.maximum / 10);
        [(packed >> 16) as u8, (packed >> 8) as u8, packed as u8]
    }
}

mod imp {
    use super::*;
    use gst::subclass::prelude::*;
    use gst_rtp::subclass::prelude::*;

    #[derive(Default)]
    pub struct PlayoutDelay {
        pub packed: AtomicU32,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PlayoutDelay {
        const NAME: &'static str = "EnzRemotePlayoutDelay";
        type Type = super::PlayoutDelay;
        type ParentType = gst_rtp::RTPHeaderExtension;
    }
    impl ObjectImpl for PlayoutDelay {}
    impl GstObjectImpl for PlayoutDelay {}
    impl ElementImpl for PlayoutDelay {}
    impl RTPHeaderExtensionImpl for PlayoutDelay {
        const URI: &'static str = super::URI;
        fn supported_flags(&self) -> gst_rtp::RTPHeaderExtensionFlags {
            gst_rtp::RTPHeaderExtensionFlags::ONE_BYTE | gst_rtp::RTPHeaderExtensionFlags::TWO_BYTE
        }
        fn max_size(&self, _: &gst::BufferRef) -> usize {
            3
        }
        fn write(
            &self,
            _: &gst::BufferRef,
            _: gst_rtp::RTPHeaderExtensionFlags,
            _: &gst::BufferRef,
            output: &mut [u8],
        ) -> Result<usize, gst::LoggableError> {
            if output.len() < 3 {
                return Err(gst::loggable_error!(
                    gst::CAT_RUST,
                    "Playout extension requires three bytes"
                ));
            }
            let packed = self.packed.load(Ordering::Relaxed);
            output[..3].copy_from_slice(&[(packed >> 16) as u8, (packed >> 8) as u8, packed as u8]);
            Ok(3)
        }
    }
}

glib::wrapper! {
    pub struct PlayoutDelay(ObjectSubclass<imp::PlayoutDelay>)
        @extends gst_rtp::RTPHeaderExtension, gst::Element, gst::Object;
}

impl PlayoutDelay {
    pub fn new(range: DelayRange) -> Self {
        let extension: Self = glib::Object::new();
        let [a, b, c] = range.bytes();
        extension.imp().packed.store(
            (u32::from(a) << 16) | (u32::from(b) << 8) | u32::from(c),
            Ordering::Relaxed,
        );
        extension
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst_rtp::prelude::*;
    use gst_rtp::subclass::prelude::*;

    #[test]
    fn extension_advertises_uri_and_writes_without_overrun() {
        gst::init().unwrap();
        let extension = PlayoutDelay::new(DelayRange::parse("0:30").unwrap().unwrap());
        assert_eq!(extension.uri().as_deref(), Some(URI));
        let buffer = gst::Buffer::new();
        let mut output = [0xff; 4];
        assert_eq!(
            extension
                .imp()
                .write(
                    &buffer,
                    gst_rtp::RTPHeaderExtensionFlags::ONE_BYTE,
                    &buffer,
                    &mut output
                )
                .unwrap(),
            3
        );
        assert_eq!(output, [0, 0, 3, 0xff]);
        assert!(
            extension
                .imp()
                .write(
                    &buffer,
                    gst_rtp::RTPHeaderExtensionFlags::ONE_BYTE,
                    &buffer,
                    &mut [0; 2]
                )
                .is_err()
        );
    }
}
