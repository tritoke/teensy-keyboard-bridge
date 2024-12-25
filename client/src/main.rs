use std::collections::HashMap;

use color_eyre::eyre::{OptionExt, Result};
use dialoguer::FuzzySelect;
use evdev::{Device, InputEventKind, Key};
use tokio::{select, io::AsyncWriteExt};
use tokio_serial::{available_ports, SerialPortBuilderExt, SerialPortInfo, SerialPortType};
use tokio_util::sync::CancellationToken;

use enumflags2::{bitflags, BitFlag, BitFlags};
use usbd_hid::descriptor::{KeyboardReport, KeyboardUsage};

#[tokio::main]
async fn main() -> Result<()> {
    let keyboard = select_input_device()?;
    let SerialPortInfo { port_name, .. } = select_serial_port()?;
    let mut serial_port = tokio_serial::new(port_name, 115200).open_native_async()?;

    let token = CancellationToken::new();
    let cloned_token = token.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        cloned_token.cancel();
    });

    let mut stream = keyboard.into_event_stream()?;
    let mut keyboard_state = KeySet::new();
    let mut buf = [0; 32];
    loop {
        let event = select! {
            _ = token.cancelled() => break,
            event = stream.next_event() => event,
        }?;
        let InputEventKind::Key(key) = event.kind() else {
            continue;
        };

        match event.value() {
            // zero is key up
            0 => keyboard_state.release_key(key),
            // one is key down
            1 => keyboard_state.press_key(key),
            // two is key hold just ignore as it doesn't change the state of pressed keys
            _ => continue,
        };

        if cfg!(debug_assertions) {
            eprintln!("\rstate = {keyboard_state:?}");
        }

        let report = shared::WhyNoDeriveDeserializeManSadFaceHere::from(keyboard_state);
        let to_send = postcard::to_slice_cobs(&report, &mut buf)?;
        serial_port.write_all(to_send).await?;
    }

    // we received Ctrl-C release all keys and exit
    let report = KeyboardReport::default();
    let to_send = postcard::to_slice_cobs(&report, &mut buf)?;
    serial_port.write_all(to_send).await?;

    Ok(())
}

fn select_input_device() -> Result<Device> {
    let mut keyboards = HashMap::new();
    for (_, device) in evdev::enumerate() {
        // if it has an "A" key its probably a keyboard
        let supported = device
            .supported_keys()
            .map_or(false, |keys| keys.contains(Key::KEY_A));
        if !supported {
            continue;
        }

        let Some(name) = device.name() else { continue };
        keyboards.insert(name.to_owned(), device);
    }

    if keyboards.len() > 1 {
        let items: Vec<_> = keyboards.keys().cloned().collect();
        let selection = FuzzySelect::new()
            .with_prompt("Which keyboard should I read events from?")
            .items(&items)
            .interact()
            .expect("Rude :(");
        Ok(keyboards
            .remove(&items[selection])
            .expect("Selected keyboard has run away :("))
    } else {
        keyboards
            .into_values()
            .next()
            .ok_or_eyre("No keyboards found, do you have permission for /dev/inputX?")
    }
}

fn select_serial_port() -> Result<SerialPortInfo> {
    let mut ports = available_ports()?;
    ports.retain(|port| port.port_type != SerialPortType::Unknown);

    if ports.len() > 1 {
        let names: Vec<_> = ports.iter().map(|info| &info.port_name).collect();
        let selection = FuzzySelect::new()
            .with_prompt("Which serial port should I send events to?")
            .items(&names)
            .interact()
            .expect("Rude :(");

        ports
            .into_iter()
            .nth(selection)
            .ok_or_eyre("Selected serial port has fled the country?")
    } else {
        ports.into_iter().next().ok_or_eyre("No serial ports?")
    }
}

#[rustfmt::skip]
#[bitflags]
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum UsbHidModifier {
    LeftControl  = 0b0000_0001,
    LeftShift    = 0b0000_0010,
    LeftAlt      = 0b0000_0100,
    LeftMeta     = 0b0000_1000,
    RightControl = 0b0001_0000,
    RightShift   = 0b0010_0000,
    RightAlt     = 0b0100_0000,
    RightMeta    = 0b1000_0000,
}

impl UsbHidModifier {
    fn from_key(key: Key) -> Option<Self> {
        match key {
            Key::KEY_LEFTCTRL => Some(Self::LeftControl),
            Key::KEY_RIGHTCTRL => Some(Self::RightControl),
            Key::KEY_LEFTSHIFT => Some(Self::LeftShift),
            Key::KEY_RIGHTSHIFT => Some(Self::RightShift),
            Key::KEY_LEFTALT => Some(Self::LeftAlt),
            Key::KEY_RIGHTALT => Some(Self::RightAlt),
            Key::KEY_LEFTMETA => Some(Self::LeftMeta),
            Key::KEY_RIGHTMETA => Some(Self::RightMeta),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct KeySet {
    modifier: BitFlags<UsbHidModifier>,
    keys: [u8; 6],
}

impl From<KeySet> for shared::WhyNoDeriveDeserializeManSadFaceHere {
    fn from(value: KeySet) -> Self {
        shared::WhyNoDeriveDeserializeManSadFaceHere {
            modifier: value.modifier.bits(),
            keys: value.keys,
        }
    }
}

impl KeySet {
    fn new() -> Self {
        Self {
            modifier: UsbHidModifier::empty(),
            keys: [0; 6],
        }
    }

    fn press_key(&mut self, key: Key) {
        if let Some(modifier) = UsbHidModifier::from_key(key) {
            self.modifier.set(modifier, true);
        } else if let Some(usage_id) = key_to_hid_usage_id(key) {
            let usage_id = usage_id as u8;
            if self.keys.contains(&usage_id) {
                return;
            }

            if let Some(slot) = self.keys.iter_mut().find(|id| **id == 0) {
                *slot = usage_id;
            }
        }
    }

    fn release_key(&mut self, key: Key) {
        if let Some(modifier) = UsbHidModifier::from_key(key) {
            self.modifier.set(modifier, false);
        } else if let Some(usage_id) = key_to_hid_usage_id(key) {
            let usage_id = usage_id as u8;
            if !self.keys.contains(&usage_id) {
                return;
            }

            if let Some(slot) = self.keys.iter_mut().find(|id| **id == usage_id) {
                *slot = 0;
            }

            self.keys.sort_by(|a, b| b.cmp(a));
        }
    }
}

fn key_to_hid_usage_id(key: Key) -> Option<KeyboardUsage> {
    let usage_id = match key {
        Key::KEY_ESC => KeyboardUsage::KeyboardEscape,
        Key::KEY_1 => KeyboardUsage::Keyboard1Exclamation,
        Key::KEY_2 => KeyboardUsage::Keyboard2At,
        Key::KEY_3 => KeyboardUsage::Keyboard3Hash,
        Key::KEY_4 => KeyboardUsage::Keyboard4Dollar,
        Key::KEY_5 => KeyboardUsage::Keyboard5Percent,
        Key::KEY_6 => KeyboardUsage::Keyboard6Caret,
        Key::KEY_7 => KeyboardUsage::Keyboard7Ampersand,
        Key::KEY_8 => KeyboardUsage::Keyboard8Asterisk,
        Key::KEY_9 => KeyboardUsage::Keyboard9OpenParens,
        Key::KEY_0 => KeyboardUsage::Keyboard0CloseParens,
        Key::KEY_MINUS => KeyboardUsage::KeyboardDashUnderscore,
        Key::KEY_EQUAL => KeyboardUsage::KeyboardEqualPlus,
        Key::KEY_BACKSPACE => KeyboardUsage::KeyboardBackspace,
        Key::KEY_TAB => KeyboardUsage::KeyboardTab,
        Key::KEY_Q => KeyboardUsage::KeyboardQq,
        Key::KEY_W => KeyboardUsage::KeyboardWw,
        Key::KEY_E => KeyboardUsage::KeyboardEe,
        Key::KEY_R => KeyboardUsage::KeyboardRr,
        Key::KEY_T => KeyboardUsage::KeyboardTt,
        Key::KEY_Y => KeyboardUsage::KeyboardYy,
        Key::KEY_U => KeyboardUsage::KeyboardUu,
        Key::KEY_I => KeyboardUsage::KeyboardIi,
        Key::KEY_O => KeyboardUsage::KeyboardOo,
        Key::KEY_P => KeyboardUsage::KeyboardPp,
        Key::KEY_LEFTBRACE => KeyboardUsage::KeyboardOpenBracketBrace,
        Key::KEY_RIGHTBRACE => KeyboardUsage::KeyboardCloseBracketBrace,
        Key::KEY_ENTER => KeyboardUsage::KeyboardEnter,
        Key::KEY_LEFTCTRL => KeyboardUsage::KeyboardLeftControl,
        Key::KEY_A => KeyboardUsage::KeyboardAa,
        Key::KEY_S => KeyboardUsage::KeyboardSs,
        Key::KEY_D => KeyboardUsage::KeyboardDd,
        Key::KEY_F => KeyboardUsage::KeyboardFf,
        Key::KEY_G => KeyboardUsage::KeyboardGg,
        Key::KEY_H => KeyboardUsage::KeyboardHh,
        Key::KEY_J => KeyboardUsage::KeyboardJj,
        Key::KEY_K => KeyboardUsage::KeyboardKk,
        Key::KEY_L => KeyboardUsage::KeyboardLl,
        Key::KEY_SEMICOLON => KeyboardUsage::KeyboardSemiColon,
        Key::KEY_APOSTROPHE => KeyboardUsage::KeyboardSingleDoubleQuote,
        Key::KEY_GRAVE => KeyboardUsage::KeyboardBacktickTilde,
        Key::KEY_LEFTSHIFT => KeyboardUsage::KeypadLeftShift,
        Key::KEY_BACKSLASH => KeyboardUsage::KeyboardNonUSHash, // UK keymap
        Key::KEY_Z => KeyboardUsage::KeyboardZz,
        Key::KEY_X => KeyboardUsage::KeyboardXx,
        Key::KEY_C => KeyboardUsage::KeyboardCc,
        Key::KEY_V => KeyboardUsage::KeyboardVv,
        Key::KEY_B => KeyboardUsage::KeyboardBb,
        Key::KEY_N => KeyboardUsage::KeyboardNn,
        Key::KEY_M => KeyboardUsage::KeyboardMm,
        Key::KEY_COMMA => KeyboardUsage::KeyboardCommaLess,
        Key::KEY_DOT => KeyboardUsage::KeyboardPeriodGreater,
        Key::KEY_SLASH => KeyboardUsage::KeyboardSlashQuestion,
        Key::KEY_RIGHTSHIFT => KeyboardUsage::KeyboardRightShift,
        Key::KEY_KPASTERISK => KeyboardUsage::KeypadMultiply,
        Key::KEY_LEFTALT => KeyboardUsage::KeyboardLeftAlt,
        Key::KEY_SPACE => KeyboardUsage::KeyboardSpacebar,
        Key::KEY_CAPSLOCK => KeyboardUsage::KeyboardCapsLock,
        Key::KEY_F1 => KeyboardUsage::KeyboardF1,
        Key::KEY_F2 => KeyboardUsage::KeyboardF2,
        Key::KEY_F3 => KeyboardUsage::KeyboardF3,
        Key::KEY_F4 => KeyboardUsage::KeyboardF4,
        Key::KEY_F5 => KeyboardUsage::KeyboardF5,
        Key::KEY_F6 => KeyboardUsage::KeyboardF6,
        Key::KEY_F7 => KeyboardUsage::KeyboardF7,
        Key::KEY_F8 => KeyboardUsage::KeyboardF8,
        Key::KEY_F9 => KeyboardUsage::KeyboardF9,
        Key::KEY_F10 => KeyboardUsage::KeyboardF10,
        Key::KEY_NUMLOCK => KeyboardUsage::KeypadNumLock,
        Key::KEY_SCROLLLOCK => KeyboardUsage::KeyboardScrollLock,
        Key::KEY_KP7 => KeyboardUsage::Keypad7Home,
        Key::KEY_KP8 => KeyboardUsage::Keypad8UpArrow,
        Key::KEY_KP9 => KeyboardUsage::Keypad9PageUp,
        Key::KEY_KPMINUS => KeyboardUsage::KeypadMinus,
        Key::KEY_KP4 => KeyboardUsage::Keypad4LeftArrow,
        Key::KEY_KP5 => KeyboardUsage::Keypad5,
        Key::KEY_KP6 => KeyboardUsage::Keypad6RightArrow,
        Key::KEY_KPPLUS => KeyboardUsage::KeypadPlus,
        Key::KEY_KP1 => KeyboardUsage::Keypad1End,
        Key::KEY_KP2 => KeyboardUsage::Keypad2DownArrow,
        Key::KEY_KP3 => KeyboardUsage::Keypad3PageDown,
        Key::KEY_KP0 => KeyboardUsage::Keypad0Insert,
        Key::KEY_KPDOT => KeyboardUsage::KeypadPeriodDelete,
        Key::KEY_ZENKAKUHANKAKU => KeyboardUsage::KeyboardLANG5,
        Key::KEY_102ND => KeyboardUsage::KeyboardNonUSSlash,
        Key::KEY_F11 => KeyboardUsage::KeyboardF11,
        Key::KEY_F12 => KeyboardUsage::KeyboardF12,
        Key::KEY_RO => KeyboardUsage::KeyboardInternational1,
        Key::KEY_KATAKANA => KeyboardUsage::KeyboardLANG3,
        Key::KEY_HIRAGANA => KeyboardUsage::KeyboardLANG4,
        Key::KEY_HENKAN => KeyboardUsage::KeyboardInternational4,
        Key::KEY_KATAKANAHIRAGANA => KeyboardUsage::KeyboardInternational2,
        Key::KEY_MUHENKAN => KeyboardUsage::KeyboardInternational5,
        Key::KEY_KPJPCOMMA => KeyboardUsage::KeyboardInternational6,
        Key::KEY_KPENTER => KeyboardUsage::KeypadEnter,
        Key::KEY_RIGHTCTRL => KeyboardUsage::KeyboardRightControl,
        Key::KEY_KPSLASH => KeyboardUsage::KeypadDivide,
        Key::KEY_SYSRQ => KeyboardUsage::KeyboardPrintScreen,
        Key::KEY_RIGHTALT => KeyboardUsage::KeyboardRightAlt,
        // Key::KEY_LINEFEED => 101,
        Key::KEY_HOME => KeyboardUsage::KeyboardHome,
        Key::KEY_UP => KeyboardUsage::KeyboardUpArrow,
        Key::KEY_PAGEUP => KeyboardUsage::KeyboardPageUp,
        Key::KEY_LEFT => KeyboardUsage::KeyboardLeftArrow,
        Key::KEY_RIGHT => KeyboardUsage::KeyboardRightArrow,
        Key::KEY_END => KeyboardUsage::KeyboardEnd,
        Key::KEY_DOWN => KeyboardUsage::KeyboardDownArrow,
        Key::KEY_PAGEDOWN => KeyboardUsage::KeyboardPageDown,
        Key::KEY_INSERT => KeyboardUsage::KeyboardInsert,
        Key::KEY_DELETE => KeyboardUsage::KeyboardDelete,
        //     Key::KEY_MACRO => 112,
        Key::KEY_MUTE => KeyboardUsage::KeyboardMute,
        Key::KEY_VOLUMEDOWN => KeyboardUsage::KeyboardVolumeDown,
        Key::KEY_VOLUMEUP => KeyboardUsage::KeyboardVolumeUp,
        Key::KEY_POWER => KeyboardUsage::KeyboardPower, /* SC System Power Down */
        Key::KEY_KPEQUAL => KeyboardUsage::KeypadEqual,
        //     Key::KEY_KPPLUSMINUS => 118,
        Key::KEY_PAUSE => KeyboardUsage::KeyboardPause,
        //     Key::KEY_SCALE => 120, /* AL Compiz Scale (Expose) */
        Key::KEY_KPCOMMA => KeyboardUsage::KeypadComma,
        Key::KEY_HANGEUL => KeyboardUsage::KeyboardLANG1,
        Key::KEY_HANJA => KeyboardUsage::KeyboardLANG2,
        Key::KEY_YEN => KeyboardUsage::KeyboardInternational3,
        Key::KEY_LEFTMETA => KeyboardUsage::KeyboardLeftGUI,
        Key::KEY_RIGHTMETA => KeyboardUsage::KeyboardRightGUI,
        Key::KEY_COMPOSE => KeyboardUsage::KeyboardApplication,
        //     Key::KEY_STOP => 128, /* AC Stop */
        Key::KEY_AGAIN => KeyboardUsage::KeyboardAgain,
        //     Key::KEY_PROPS => 130, /* AC Properties */
        // Key::KEY_UNDO => KeyboardUsage::KeyboardUndo,  /* AC Undo */
        Key::KEY_FRONT => KeyboardUsage::KeyboardSelect,
        // Key::KEY_COPY => KeyboardUsage::KeyboardCopy,  /* AC Copy */
        // Key::KEY_OPEN => 134,  /* AC Open */
        // A this point I got bored / had a headache...
        //     Key::KEY_PASTE => 135, /* AC Paste */
        //     Key::KEY_FIND => 136,  /* AC Search */
        //     Key::KEY_CUT => 137,   /* AC Cut */
        //     Key::KEY_HELP => 138,  /* AL Integrated Help Center */
        //     Key::KEY_MENU => 139,  /* Menu (show menu) */
        //     Key::KEY_CALC => 140,  /* AL Calculator */
        //     Key::KEY_SETUP => 141,
        //     Key::KEY_SLEEP => 142,  /* SC System Sleep */
        //     Key::KEY_WAKEUP => 143, /* System Wake Up */
        //     Key::KEY_FILE => 144,   /* AL Local Machine Browser */
        //     Key::KEY_SENDFILE = 145,
        //     Key::KEY_DELETEFILE => 146,
        //     Key::KEY_XFER => 147,
        //     Key::KEY_PROG1 => 148,
        //     Key::KEY_PROG2 => 149,
        //     Key::KEY_WWW = 150, /* AL Internet Browser */
        //     Key::KEY_MSDOS => 151,
        //     Key::KEY_COFFEE => 152, /* AL Terminal Lock/Screensaver */
        //     Key::KEY_DIRECTION => 153,
        //     Key::KEY_ROTATE_DISPLAY = 153,
        //     Key::KEY_CYCLEWINDOWS = 154,
        //     Key::KEY_MAIL = 155,
        //     Key::KEY_BOOKMARKS = 156, /* AC Bookmarks */
        //     Key::KEY_COMPUTER = 157,
        //     Key::KEY_BACK = 158,    /* AC Back */
        //     Key::KEY_FORWARD = 159, /* AC Forward */
        //     Key::KEY_CLOSECD = 160,
        //     Key::KEY_EJECTCD = 161,
        //     Key::KEY_EJECTCLOSECD = 162,
        //     Key::KEY_NEXTSONG = 163,
        //     Key::KEY_PLAYPAUSE = 164,
        //     Key::KEY_PREVIOUSSONG = 165,
        //     Key::KEY_STOPCD = 166,
        //     Key::KEY_RECORD = 167,
        //     Key::KEY_REWIND = 168,
        //     Key::KEY_PHONE = 169, /* Media Select Telephone */
        //     Key::KEY_ISO = 170,
        //     Key::KEY_CONFIG = 171,   /* AL Consumer Control Configuration */
        //     Key::KEY_HOMEPAGE = 172, /* AC Home */
        //     Key::KEY_REFRESH = 173,  /* AC Refresh */
        //     Key::KEY_EXIT = 174,     /* AC Exit */
        //     Key::KEY_MOVE = 175,
        //     Key::KEY_EDIT = 176,
        //     Key::KEY_SCROLLUP = 177,
        //     Key::KEY_SCROLLDOWN = 178,
        //     Key::KEY_KPLEFTPAREN = 179,
        //     Key::KEY_KPRIGHTPAREN = 180,
        //     Key::KEY_NEW = 181,  /* AC New */
        //     Key::KEY_REDO = 182, /* AC Redo/Repeat */
        Key::KEY_F13 => KeyboardUsage::KeyboardF13,
        Key::KEY_F14 => KeyboardUsage::KeyboardF14,
        Key::KEY_F15 => KeyboardUsage::KeyboardF15,
        Key::KEY_F16 => KeyboardUsage::KeyboardF16,
        Key::KEY_F17 => KeyboardUsage::KeyboardF17,
        Key::KEY_F18 => KeyboardUsage::KeyboardF18,
        Key::KEY_F19 => KeyboardUsage::KeyboardF19,
        Key::KEY_F20 => KeyboardUsage::KeyboardF20,
        Key::KEY_F21 => KeyboardUsage::KeyboardF21,
        Key::KEY_F22 => KeyboardUsage::KeyboardF22,
        Key::KEY_F23 => KeyboardUsage::KeyboardF23,
        Key::KEY_F24 => KeyboardUsage::KeyboardF24,
        //     Key::KEY_PLAYCD = 200,
        //     Key::KEY_PAUSECD = 201,
        //     Key::KEY_PROG3 = 202,
        //     Key::KEY_PROG4 = 203,
        //     Key::KEY_DASHBOARD = 204, /* AL Dashboard */
        //     Key::KEY_SUSPEND = 205,
        //     Key::KEY_CLOSE = 206, /* AC Close */
        //     Key::KEY_PLAY = 207,
        //     Key::KEY_FASTFORWARD = 208,
        //     Key::KEY_BASSBOOST = 209,
        //     Key::KEY_PRINT = 210, /* AC Print */
        //     Key::KEY_HP = 211,
        //     Key::KEY_CAMERA = 212,
        //     Key::KEY_SOUND = 213,
        //     Key::KEY_QUESTION = 214,
        //     Key::KEY_EMAIL = 215,
        //     Key::KEY_CHAT = 216,
        //     Key::KEY_SEARCH = 217,
        //     Key::KEY_CONNECT = 218,
        //     Key::KEY_FINANCE = 219,
        //     Key::KEY_SPORT = 220,
        //     Key::KEY_SHOP = 221,
        Key::KEY_ALTERASE => KeyboardUsage::KeyboardAlternateErase,
        //     Key::KEY_CANCEL = 223,
        //     Key::KEY_BRIGHTNESSDOWN = 224,
        //     Key::KEY_BRIGHTNESSUP = 225,
        //     Key::KEY_MEDIA = 226,
        //     Key::KEY_SWITCHVIDEOMODE = 227,
        //     Key::KEY_KBDILLUMTOGGLE = 228,
        //     Key::KEY_KBDILLUMDOWN = 229,
        //     Key::KEY_KBDILLUMUP = 230,
        //     Key::KEY_SEND = 231,
        //     Key::KEY_REPLY = 232,
        //     Key::KEY_FORWARDMAIL = 233,
        //     Key::KEY_SAVE = 234,
        //     Key::KEY_DOCUMENTS = 235,
        //     Key::KEY_BATTERY = 236,
        //     Key::KEY_BLUETOOTH = 237,
        //     Key::KEY_WLAN = 238,
        //     Key::KEY_UWB = 239,
        //     Key::KEY_UNKNOWN = 240,
        //     Key::KEY_VIDEO_NEXT = 241,
        //     Key::KEY_VIDEO_PREV = 242,
        //     Key::KEY_BRIGHTNESS_CYCLE = 243,
        //     Key::KEY_BRIGHTNESS_AUTO = 244,
        //     Key::KEY_DISPLAY_OFF = 245,
        //     Key::KEY_WWAN = 246,
        //     Key::KEY_RFKILL = 247,
        //     Key::KEY_MICMUTE = 248,
        //     Key::BTN_0 = 0x100,
        //     Key::BTN_1 = 0x101,
        //     Key::BTN_2 = 0x102,
        //     Key::BTN_3 = 0x103,
        //     Key::BTN_4 = 0x104,
        //     Key::BTN_5 = 0x105,
        //     Key::BTN_6 = 0x106,
        //     Key::BTN_7 = 0x107,
        //     Key::BTN_8 = 0x108,
        //     Key::BTN_9 = 0x109,
        //     Key::BTN_LEFT = 0x110,
        //     Key::BTN_RIGHT = 0x111,
        //     Key::BTN_MIDDLE = 0x112,
        //     Key::BTN_SIDE = 0x113,
        //     Key::BTN_EXTRA = 0x114,
        //     Key::BTN_FORWARD = 0x115,
        //     Key::BTN_BACK = 0x116,
        //     Key::BTN_TASK = 0x117,
        //     Key::BTN_TRIGGER = 0x120,
        //     Key::BTN_THUMB = 0x121,
        //     Key::BTN_THUMB2 = 0x122,
        //     Key::BTN_TOP = 0x123,
        //     Key::BTN_TOP2 = 0x124,
        //     Key::BTN_PINKIE = 0x125,
        //     Key::BTN_BASE = 0x126,
        //     Key::BTN_BASE2 = 0x127,
        //     Key::BTN_BASE3 = 0x128,
        //     Key::BTN_BASE4 = 0x129,
        //     Key::BTN_BASE5 = 0x12a,
        //     Key::BTN_BASE6 = 0x12b,
        //     Key::BTN_DEAD = 0x12f,
        //     Key::BTN_SOUTH = 0x130,
        //     Key::BTN_EAST = 0x131,
        //     Key::BTN_C = 0x132,
        //     Key::BTN_NORTH = 0x133,
        //     Key::BTN_WEST = 0x134,
        //     Key::BTN_Z = 0x135,
        //     Key::BTN_TL = 0x136,
        //     Key::BTN_TR = 0x137,
        //     Key::BTN_TL2 = 0x138,
        //     Key::BTN_TR2 = 0x139,
        //     Key::BTN_SELECT = 0x13a,
        //     Key::BTN_START = 0x13b,
        //     Key::BTN_MODE = 0x13c,
        //     Key::BTN_THUMBL = 0x13d,
        //     Key::BTN_THUMBR = 0x13e,
        //     Key::BTN_TOOL_PEN = 0x140,
        //     Key::BTN_TOOL_RUBBER = 0x141,
        //     Key::BTN_TOOL_BRUSH = 0x142,
        //     Key::BTN_TOOL_PENCIL = 0x143,
        //     Key::BTN_TOOL_AIRBRUSH = 0x144,
        //     Key::BTN_TOOL_FINGER = 0x145,
        //     Key::BTN_TOOL_MOUSE = 0x146,
        //     Key::BTN_TOOL_LENS = 0x147,
        //     Key::BTN_TOOL_QUINTTAP = 0x148, /* Five fingers on trackpad */
        //     Key::BTN_TOUCH = 0x14a,
        //     Key::BTN_STYLUS = 0x14b,
        //     Key::BTN_STYLUS2 = 0x14c,
        //     Key::BTN_TOOL_DOUBLETAP = 0x14d,
        //     Key::BTN_TOOL_TRIPLETAP = 0x14e,
        //     Key::BTN_TOOL_QUADTAP = 0x14f, /* Four fingers on trackpad */
        //     Key::BTN_GEAR_DOWN = 0x150,
        //     Key::BTN_GEAR_UP = 0x151,
        //     Key::KEY_OK = 0x160,
        //     Key::KEY_SELECT = 0x161,
        //     Key::KEY_GOTO = 0x162,
        //     Key::KEY_CLEAR = 0x163,
        //     Key::KEY_POWER2 = 0x164,
        //     Key::KEY_OPTION = 0x165,
        //     Key::KEY_INFO = 0x166, /* AL OEM Features/Tips/Tutorial */
        //     Key::KEY_TIME = 0x167,
        //     Key::KEY_VENDOR = 0x168,
        //     Key::KEY_ARCHIVE = 0x169,
        //     Key::KEY_PROGRAM = 0x16a, /* Media Select Program Guide */
        //     Key::KEY_CHANNEL = 0x16b,
        //     Key::KEY_FAVORITES = 0x16c,
        //     Key::KEY_EPG = 0x16d,
        //     Key::KEY_PVR = 0x16e, /* Media Select Home */
        //     Key::KEY_MHP = 0x16f,
        //     Key::KEY_LANGUAGE = 0x170,
        //     Key::KEY_TITLE = 0x171,
        //     Key::KEY_SUBTITLE = 0x172,
        //     Key::KEY_ANGLE = 0x173,
        //     Key::KEY_ZOOM = 0x174,
        //     Key::KEY_FULL_SCREEN = 0x174,
        //     Key::KEY_MODE = 0x175,
        //     Key::KEY_KEYBOARD = 0x176,
        //     Key::KEY_SCREEN = 0x177,
        //     Key::KEY_PC = 0x178,   /* Media Select Computer */
        //     Key::KEY_TV = 0x179,   /* Media Select TV */
        //     Key::KEY_TV2 = 0x17a,  /* Media Select Cable */
        //     Key::KEY_VCR = 0x17b,  /* Media Select VCR */
        //     Key::KEY_VCR2 = 0x17c, /* VCR Plus */
        //     Key::KEY_SAT = 0x17d,  /* Media Select Satellite */
        //     Key::KEY_SAT2 = 0x17e,
        //     Key::KEY_CD = 0x17f,   /* Media Select CD */
        //     Key::KEY_TAPE = 0x180, /* Media Select Tape */
        //     Key::KEY_RADIO = 0x181,
        //     Key::KEY_TUNER = 0x182, /* Media Select Tuner */
        //     Key::KEY_PLAYER = 0x183,
        //     Key::KEY_TEXT = 0x184,
        //     Key::KEY_DVD = 0x185, /* Media Select DVD */
        //     Key::KEY_AUX = 0x186,
        //     Key::KEY_MP3 = 0x187,
        //     Key::KEY_AUDIO = 0x188, /* AL Audio Browser */
        //     Key::KEY_VIDEO = 0x189, /* AL Movie Browser */
        //     Key::KEY_DIRECTORY = 0x18a,
        //     Key::KEY_LIST = 0x18b,
        //     Key::KEY_MEMO = 0x18c, /* Media Select Messages */
        //     Key::KEY_CALENDAR = 0x18d,
        //     Key::KEY_RED = 0x18e,
        //     Key::KEY_GREEN = 0x18f,
        //     Key::KEY_YELLOW = 0x190,
        //     Key::KEY_BLUE = 0x191,
        //     Key::KEY_CHANNELUP = 0x192,   /* Channel Increment */
        //     Key::KEY_CHANNELDOWN = 0x193, /* Channel Decrement */
        //     Key::KEY_FIRST = 0x194,
        //     Key::KEY_LAST = 0x195, /* Recall Last */
        //     Key::KEY_AB = 0x196,
        //     Key::KEY_NEXT = 0x197,
        //     Key::KEY_RESTART = 0x198,
        //     Key::KEY_SLOW = 0x199,
        //     Key::KEY_SHUFFLE = 0x19a,
        //     Key::KEY_BREAK = 0x19b,
        //     Key::KEY_PREVIOUS = 0x19c,
        //     Key::KEY_DIGITS = 0x19d,
        //     Key::KEY_TEEN = 0x19e,
        //     Key::KEY_TWEN = 0x19f,
        //     Key::KEY_VIDEOPHONE = 0x1a0,     /* Media Select Video Phone */
        //     Key::KEY_GAMES = 0x1a1,          /* Media Select Games */
        //     Key::KEY_ZOOMIN = 0x1a2,         /* AC Zoom In */
        //     Key::KEY_ZOOMOUT = 0x1a3,        /* AC Zoom Out */
        //     Key::KEY_ZOOMRESET = 0x1a4,      /* AC Zoom */
        //     Key::KEY_WORDPROCESSOR = 0x1a5,  /* AL Word Processor */
        //     Key::KEY_EDITOR = 0x1a6,         /* AL Text Editor */
        //     Key::KEY_SPREADSHEET = 0x1a7,    /* AL Spreadsheet */
        //     Key::KEY_GRAPHICSEDITOR = 0x1a8, /* AL Graphics Editor */
        //     Key::KEY_PRESENTATION = 0x1a9,   /* AL Presentation App */
        //     Key::KEY_DATABASE = 0x1aa,       /* AL Database App */
        //     Key::KEY_NEWS = 0x1ab,           /* AL Newsreader */
        //     Key::KEY_VOICEMAIL = 0x1ac,      /* AL Voicemail */
        //     Key::KEY_ADDRESSBOOK = 0x1ad,    /* AL Contacts/Address Book */
        //     Key::KEY_MESSENGER = 0x1ae,      /* AL Instant Messaging */
        //     Key::KEY_DISPLAYTOGGLE = 0x1af,  /* Turn display (LCD) on and off */
        //     Key::KEY_SPELLCHECK = 0x1b0,     /* AL Spell Check */
        //     Key::KEY_LOGOFF = 0x1b1,         /* AL Logoff */
        //     Key::KEY_DOLLAR = 0x1b2,
        //     Key::KEY_EURO = 0x1b3,
        //     Key::KEY_FRAMEBACK = 0x1b4, /* Consumer - transport controls */
        //     Key::KEY_FRAMEFORWARD = 0x1b5,
        //     Key::KEY_CONTEXT_MENU = 0x1b6,   /* GenDesc - system context menu */
        //     Key::KEY_MEDIA_REPEAT = 0x1b7,   /* Consumer - transport control */
        //     Key::KEY_10CHANNELSUP = 0x1b8,   /* 10 channels up (10+) */
        //     Key::KEY_10CHANNELSDOWN = 0x1b9, /* 10 channels down (10-) */
        //     Key::KEY_IMAGES = 0x1ba,         /* AL Image Browser */
        //     Key::KEY_DEL_EOL = 0x1c0,
        //     Key::KEY_DEL_EOS = 0x1c1,
        //     Key::KEY_INS_LINE = 0x1c2,
        //     Key::KEY_DEL_LINE = 0x1c3,
        //     Key::KEY_FN = 0x1d0,
        //     Key::KEY_FN_ESC = 0x1d1,
        //     Key::KEY_FN_F1 = 0x1d2,
        //     Key::KEY_FN_F2 = 0x1d3,
        //     Key::KEY_FN_F3 = 0x1d4,
        //     Key::KEY_FN_F4 = 0x1d5,
        //     Key::KEY_FN_F5 = 0x1d6,
        //     Key::KEY_FN_F6 = 0x1d7,
        //     Key::KEY_FN_F7 = 0x1d8,
        //     Key::KEY_FN_F8 = 0x1d9,
        //     Key::KEY_FN_F9 = 0x1da,
        //     Key::KEY_FN_F10 = 0x1db,
        //     Key::KEY_FN_F11 = 0x1dc,
        //     Key::KEY_FN_F12 = 0x1dd,
        //     Key::KEY_FN_1 = 0x1de,
        //     Key::KEY_FN_2 = 0x1df,
        //     Key::KEY_FN_D = 0x1e0,
        //     Key::KEY_FN_E = 0x1e1,
        //     Key::KEY_FN_F = 0x1e2,
        //     Key::KEY_FN_S = 0x1e3,
        //     Key::KEY_FN_B = 0x1e4,
        //     Key::KEY_BRL_DOT1 = 0x1f1,
        //     Key::KEY_BRL_DOT2 = 0x1f2,
        //     Key::KEY_BRL_DOT3 = 0x1f3,
        //     Key::KEY_BRL_DOT4 = 0x1f4,
        //     Key::KEY_BRL_DOT5 = 0x1f5,
        //     Key::KEY_BRL_DOT6 = 0x1f6,
        //     Key::KEY_BRL_DOT7 = 0x1f7,
        //     Key::KEY_BRL_DOT8 = 0x1f8,
        //     Key::KEY_BRL_DOT9 = 0x1f9,
        //     Key::KEY_BRL_DOT10 = 0x1fa,
        //     Key::KEY_NUMERIC_0 = 0x200, /* used by phones, remote controls, */
        //     Key::KEY_NUMERIC_1 = 0x201, /* and other keypads */
        //     Key::KEY_NUMERIC_2 = 0x202,
        //     Key::KEY_NUMERIC_3 = 0x203,
        //     Key::KEY_NUMERIC_4 = 0x204,
        //     Key::KEY_NUMERIC_5 = 0x205,
        //     Key::KEY_NUMERIC_6 = 0x206,
        //     Key::KEY_NUMERIC_7 = 0x207,
        //     Key::KEY_NUMERIC_8 = 0x208,
        //     Key::KEY_NUMERIC_9 = 0x209,
        //     Key::KEY_NUMERIC_STAR = 0x20a,
        //     Key::KEY_NUMERIC_POUND = 0x20b,
        //     Key::KEY_NUMERIC_A = 0x20c, /* Phone key A - HUT Telephony 0xb9 */
        //     Key::KEY_NUMERIC_B = 0x20d,
        //     Key::KEY_NUMERIC_C = 0x20e,
        //     Key::KEY_NUMERIC_D = 0x20f,
        //     Key::KEY_CAMERA_FOCUS = 0x210,
        //     Key::KEY_WPS_BUTTON = 0x211,      /* WiFi Protected Setup key */
        //     Key::KEY_TOUCHPAD_TOGGLE = 0x212, /* Request switch touchpad on or off */
        //     Key::KEY_TOUCHPAD_ON = 0x213,
        //     Key::KEY_TOUCHPAD_OFF = 0x214,
        //     Key::KEY_CAMERA_ZOOMIN = 0x215,
        //     Key::KEY_CAMERA_ZOOMOUT = 0x216,
        //     Key::KEY_CAMERA_UP = 0x217,
        //     Key::KEY_CAMERA_DOWN = 0x218,
        //     Key::KEY_CAMERA_LEFT = 0x219,
        //     Key::KEY_CAMERA_RIGHT = 0x21a,
        //     Key::KEY_ATTENDANT_ON = 0x21b,
        //     Key::KEY_ATTENDANT_OFF = 0x21c,
        //     Key::KEY_ATTENDANT_TOGGLE = 0x21d, /* Attendant call on or off */
        //     Key::KEY_LIGHTS_TOGGLE = 0x21e,    /* Reading light on or off */
        //     Key::BTN_DPAD_UP = 0x220,
        //     Key::BTN_DPAD_DOWN = 0x221,
        //     Key::BTN_DPAD_LEFT = 0x222,
        //     Key::BTN_DPAD_RIGHT = 0x223,
        //     Key::KEY_ALS_TOGGLE = 0x230,   /* Ambient light sensor */
        //     Key::KEY_BUTTONCONFIG = 0x240, /* AL Button Configuration */
        //     Key::KEY_TASKMANAGER = 0x241,  /* AL Task/Project Manager */
        //     Key::KEY_JOURNAL = 0x242,      /* AL Log/Journal/Timecard */
        //     Key::KEY_CONTROLPANEL = 0x243, /* AL Control Panel */
        //     Key::KEY_APPSELECT = 0x244,    /* AL Select Task/Application */
        //     Key::KEY_SCREENSAVER = 0x245,  /* AL Screen Saver */
        //     Key::KEY_VOICECOMMAND = 0x246, /* Listening Voice Command */
        //     Key::KEY_ASSISTANT = 0x247,
        //     Key::KEY_KBD_LAYOUT_NEXT = 0x248,
        //     Key::KEY_BRIGHTNESS_MIN = 0x250, /* Set Brightness to Minimum */
        //     Key::KEY_BRIGHTNESS_MAX = 0x251, /* Set Brightness to Maximum */
        //     Key::KEY_KBDINPUTASSIST_PREV = 0x260,
        //     Key::KEY_KBDINPUTASSIST_NEXT = 0x261,
        //     Key::KEY_KBDINPUTASSIST_PREVGROUP = 0x262,
        //     Key::KEY_KBDINPUTASSIST_NEXTGROUP = 0x263,
        //     Key::KEY_KBDINPUTASSIST_ACCEPT = 0x264,
        //     Key::KEY_KBDINPUTASSIST_CANCEL = 0x265,
        //     Key::KEY_RIGHT_UP = 0x266,
        //     Key::KEY_RIGHT_DOWN = 0x267,
        //     Key::KEY_LEFT_UP = 0x268,
        //     Key::KEY_LEFT_DOWN = 0x269,
        //     Key::KEY_ROOT_MENU = 0x26a,
        //     Key::KEY_MEDIA_TOP_MENU = 0x26b,
        //     Key::KEY_NUMERIC_11 = 0x26c,
        //     Key::KEY_NUMERIC_12 = 0x26d,
        //     Key::KEY_AUDIO_DESC = 0x26e,
        //     Key::KEY_3D_MODE = 0x26f,
        //     Key::KEY_NEXT_FAVORITE = 0x270,
        //     Key::KEY_STOP_RECORD = 0x271,
        //     Key::KEY_PAUSE_RECORD = 0x272,
        //     Key::KEY_VOD = 0x273, /* Video on Demand */
        //     Key::KEY_UNMUTE = 0x274,
        //     Key::KEY_FASTREVERSE = 0x275,
        //     Key::KEY_SLOWREVERSE = 0x276,
        //     Key::KEY_DATA = 0x277,
        //     Key::KEY_ONSCREEN_KEYBOARD = 0x278,
        //     Key::KEY_PRIVACY_SCREEN_TOGGLE = 0x279,
        //     Key::KEY_SELECTIVE_SCREENSHOT = 0x27a,
        //     Key::BTN_TRIGGER_HAPPY1 = 0x2c0,
        //     Key::BTN_TRIGGER_HAPPY2 = 0x2c1,
        //     Key::BTN_TRIGGER_HAPPY3 = 0x2c2,
        //     Key::BTN_TRIGGER_HAPPY4 = 0x2c3,
        //     Key::BTN_TRIGGER_HAPPY5 = 0x2c4,
        //     Key::BTN_TRIGGER_HAPPY6 = 0x2c5,
        //     Key::BTN_TRIGGER_HAPPY7 = 0x2c6,
        //     Key::BTN_TRIGGER_HAPPY8 = 0x2c7,
        //     Key::BTN_TRIGGER_HAPPY9 = 0x2c8,
        //     Key::BTN_TRIGGER_HAPPY10 = 0x2c9,
        //     Key::BTN_TRIGGER_HAPPY11 = 0x2ca,
        //     Key::BTN_TRIGGER_HAPPY12 = 0x2cb,
        //     Key::BTN_TRIGGER_HAPPY13 = 0x2cc,
        //     Key::BTN_TRIGGER_HAPPY14 = 0x2cd,
        //     Key::BTN_TRIGGER_HAPPY15 = 0x2ce,
        //     Key::BTN_TRIGGER_HAPPY16 = 0x2cf,
        //     Key::BTN_TRIGGER_HAPPY17 = 0x2d0,
        //     Key::BTN_TRIGGER_HAPPY18 = 0x2d1,
        //     Key::BTN_TRIGGER_HAPPY19 = 0x2d2,
        //     Key::BTN_TRIGGER_HAPPY20 = 0x2d3,
        //     Key::BTN_TRIGGER_HAPPY21 = 0x2d4,
        //     Key::BTN_TRIGGER_HAPPY22 = 0x2d5,
        //     Key::BTN_TRIGGER_HAPPY23 = 0x2d6,
        //     Key::BTN_TRIGGER_HAPPY24 = 0x2d7,
        //     Key::BTN_TRIGGER_HAPPY25 = 0x2d8,
        //     Key::BTN_TRIGGER_HAPPY26 = 0x2d9,
        //     Key::BTN_TRIGGER_HAPPY27 = 0x2da,
        //     Key::BTN_TRIGGER_HAPPY28 = 0x2db,
        //     Key::BTN_TRIGGER_HAPPY29 = 0x2dc,
        //     Key::BTN_TRIGGER_HAPPY30 = 0x2dd,
        //     Key::BTN_TRIGGER_HAPPY31 = 0x2de,
        //     Key::BTN_TRIGGER_HAPPY32 = 0x2df,
        //     Key::BTN_TRIGGER_HAPPY33 = 0x2e0,
        //     Key::BTN_TRIGGER_HAPPY34 = 0x2e1,
        //     Key::BTN_TRIGGER_HAPPY35 = 0x2e2,
        //     Key::BTN_TRIGGER_HAPPY36 = 0x2e3,
        //     Key::BTN_TRIGGER_HAPPY37 = 0x2e4,
        //     Key::BTN_TRIGGER_HAPPY38 = 0x2e5,
        //     Key::BTN_TRIGGER_HAPPY39 = 0x2e6,
        //     Key::BTN_TRIGGER_HAPPY40 = 0x2e7,
        _ => return None,
    };

    Some(usage_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_standard_modifiers() {
        let mut keyset = KeySet::new();
        let presses = [
            Key::KEY_LEFTCTRL,
            Key::KEY_LEFTSHIFT,
            Key::KEY_LEFTALT,
            Key::KEY_LEFTMETA,
            Key::KEY_RIGHTCTRL,
            Key::KEY_RIGHTSHIFT,
            Key::KEY_RIGHTALT,
            Key::KEY_RIGHTMETA,
        ];
        for (i, key) in presses.into_iter().enumerate() {
            keyset.press_key(key);
            assert_eq!(keyset.modifier.bits(), u8::MAX >> (7 - i));
        }

        for (i, key) in presses.into_iter().rev().enumerate() {
            keyset.release_key(key);
            assert_eq!(
                keyset.modifier.bits(),
                u8::MAX.checked_shr(i as u32 + 1).unwrap_or(0)
            );
        }
    }

    #[test]
    fn test_press_a_release_a() {
        let mut keyset = KeySet::new();
        let key = Key::KEY_A;
        keyset.press_key(key);
        assert_eq!(
            keyset.keys,
            [KeyboardUsage::KeyboardAa as u8, 0, 0, 0, 0, 0]
        );
        keyset.release_key(key);
        assert_eq!(keyset.keys, [0; 6]);
    }

    #[test]
    fn test_press_ab_release_ba() {
        let mut keyset = KeySet::new();
        let a = Key::KEY_A;
        let b = Key::KEY_B;
        keyset.press_key(a);
        assert_eq!(
            keyset.keys,
            [KeyboardUsage::KeyboardAa as u8, 0, 0, 0, 0, 0]
        );
        keyset.press_key(b);
        assert_eq!(
            keyset.keys,
            [
                KeyboardUsage::KeyboardAa as u8,
                KeyboardUsage::KeyboardBb as u8,
                0,
                0,
                0,
                0
            ]
        );
        keyset.release_key(b);
        assert_eq!(
            keyset.keys,
            [KeyboardUsage::KeyboardAa as u8, 0, 0, 0, 0, 0]
        );
        keyset.release_key(a);
        assert_eq!(keyset.keys, [0; 6]);
    }

    #[test]
    fn test_press_abcdefg_release_abcdefg() {
        let mut keyset = KeySet::new();
        let a = KeyboardUsage::KeyboardAa as u8;

        keyset.press_key(Key::KEY_A);
        keyset.press_key(Key::KEY_B);
        keyset.press_key(Key::KEY_C);
        keyset.press_key(Key::KEY_D);
        keyset.press_key(Key::KEY_E);
        keyset.press_key(Key::KEY_F);
        assert_eq!(keyset.keys, [a, a + 1, a + 2, a + 3, a + 4, a + 5]);
        keyset.press_key(Key::KEY_G);
        assert_eq!(keyset.keys, [a, a + 1, a + 2, a + 3, a + 4, a + 5]);
        keyset.release_key(Key::KEY_G);
        assert_eq!(keyset.keys, [a, a + 1, a + 2, a + 3, a + 4, a + 5]);
        keyset.release_key(Key::KEY_F);
        assert_eq!(keyset.keys, [a + 4, a + 3, a + 2, a + 1, a, 0]);
        keyset.release_key(Key::KEY_E);
        keyset.release_key(Key::KEY_D);
        keyset.release_key(Key::KEY_C);
        keyset.release_key(Key::KEY_B);
        keyset.release_key(Key::KEY_A);
        assert_eq!(keyset.keys, [0; 6]);
    }
}
