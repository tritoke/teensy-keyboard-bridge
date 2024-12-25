#![no_std]

use serde::{Deserialize, Serialize};
use usbd_hid::descriptor::KeyboardReport;

// A struct to pass a KeySet across to the firmware...
#[derive(Clone, Copy, Deserialize, Serialize)]
pub struct WhyNoDeriveDeserializeManSadFaceHere {
    pub modifier: u8,
    pub keys: [u8; 6],
}

impl From<WhyNoDeriveDeserializeManSadFaceHere> for KeyboardReport {
    fn from(value: WhyNoDeriveDeserializeManSadFaceHere) -> Self {
        KeyboardReport {
            modifier: value.modifier,
            reserved: 0,
            leds: 0,
            keycodes: value.keys,
        }
    }
}
