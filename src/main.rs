use clap::{Parser, Subcommand};
use retry::{OperationResult, delay::Fixed, retry};
use rusb::UsbContext;
use std::fmt;
use std::time::Duration;

const VENDOR_ID: u16 = 0x191a;
const PRODUCT_ID: u16 = 0x8003;
const ENDPOINT_OUT: u8 = 0x01;
const TIMEOUT_MS: u64 = 1000;
const REPORT_LEN: usize = 8;
// Allow concurrent commands ~1s for the kernel to release the HID interface.
const BUSY_RETRY_ATTEMPTS: usize = 20;
const BUSY_RETRY_DELAY_MS: u64 = 50;

const COMMAND_VERSION: u8 = 0x00;
const COMMAND_ID: u8 = 0x00;

const COLOR_RED: u8 = 0;
const COLOR_YELLOW: u8 = 1;
const COLOR_GREEN: u8 = 2;
const COLOR_BLUE: u8 = 3;
const COLOR_WHITE: u8 = 4;

const LED_KEEP: u8 = 0x0F;
const LED_OFF: u8 = 0x00;
const LED_KEEP_PAIR: u8 = (LED_KEEP << 4) | LED_KEEP;
const LED_KEEP_HIGH: u8 = LED_KEEP << 4;

const BUZZER_KEEP: u8 = 0x0F;
const BUZZER_OFF: u8 = 0x00;
const BUZZER_PITCH_DEFAULT_A: u8 = 0x0E;
const BUZZER_PITCH_DEFAULT_B: u8 = 0x0F;
const PITCH_OFF: u8 = 0x00;

const COLOR_HELP: &str = "Color id or alias: 0-4 | red yellow green blue white";
const LED_HELP: &str = "LED state nibble or alias: 0-15 | led_off led_on led_pattern1 led_pattern2 led_pattern3 led_pattern4 led_keep";
const BUZZER_HELP: &str = "Buzzer pattern nibble or alias: 0-15 | buzz_off buzz_on buzz_pattern1 buzz_pattern2 buzz_pattern3 buzz_pattern4 buzzer_keep";
const LIMIT_HELP: &str = "Buzzer limit nibble: 0=continuous, 1-15=timed";
const PITCH_HELP: &str = "Pitch nibble (0-15) - supply A and B together to override defaults";
const REPORT_HELP: &str = "8-byte HID report (decimal or 0x-prefixed hex values)";

type ControlResult<T> = Result<T, ControlError>;

#[derive(Parser)]
#[command(
    name = "ptltectl",
    author,
    version,
    about = "Control the Patlite LR6-USB tower"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Set a single LED's state (color + pattern)
    Light {
        #[arg(value_parser = parse_color, help = COLOR_HELP)]
        color: u8,
        #[arg(value_parser = parse_led_state, help = LED_HELP)]
        state: u8,
    },
    /// Set the entire tower: red yellow green blue white
    Tower {
        #[arg(value_parser = parse_led_state, help = LED_HELP)]
        red: u8,
        #[arg(value_parser = parse_led_state, help = LED_HELP)]
        yellow: u8,
        #[arg(value_parser = parse_led_state, help = LED_HELP)]
        green: u8,
        #[arg(value_parser = parse_led_state, help = LED_HELP)]
        blue: u8,
        #[arg(value_parser = parse_led_state, help = LED_HELP)]
        white: u8,
    },
    /// Control the buzzer with optional explicit pitches
    Buzzer {
        #[arg(value_parser = parse_buzzer_pattern, help = BUZZER_HELP)]
        pattern: u8,
        #[arg(value_parser = parse_nibble, help = LIMIT_HELP)]
        limit: u8,
        #[arg(value_parser = parse_nibble, help = PITCH_HELP)]
        pitch_a: Option<u8>,
        #[arg(value_parser = parse_nibble, help = PITCH_HELP)]
        pitch_b: Option<u8>,
    },
    /// Turn everything off
    Reset,
    /// Send a raw 8-byte HID report (decimal or 0x-prefixed hex)
    Report {
        #[arg(
            value_parser = parse_byte,
            num_args = REPORT_LEN,
            value_name = "BYTE",
            help = REPORT_HELP
        )]
        bytes: Vec<u8>,
    },
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => println!("ok"),
        Err(err) => {
            eprintln!("Error: {err}");
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> ControlResult<()> {
    match cli.command {
        Command::Light { color, state } => {
            let (led_ry, led_gb, led_w) = assemble_leds(color, state)?;
            send_report(build_report(BUZZER_KEEP, 0, led_ry, led_gb, led_w))
        }
        Command::Tower {
            red,
            yellow,
            green,
            blue,
            white,
        } => {
            let led_ry = (nibble(red) << 4) | nibble(yellow);
            let led_gb = (nibble(green) << 4) | nibble(blue);
            let led_w = nibble(white) << 4;
            send_report(build_report(BUZZER_KEEP, 0, led_ry, led_gb, led_w))
        }
        Command::Buzzer {
            pattern,
            limit,
            pitch_a,
            pitch_b,
        } => {
            let (pitch_a, pitch_b) = match (pitch_a, pitch_b) {
                (Some(a), Some(b)) => (a, b),
                (None, None) => (BUZZER_PITCH_DEFAULT_A, BUZZER_PITCH_DEFAULT_B),
                _ => {
                    return Err(ControlError::InvalidArg(
                        "pitch values must include both A and B nibbles".into(),
                    ));
                }
            };

            let buzzer = (nibble(limit) << 4) | nibble(pattern);
            let pitch = (nibble(pitch_a) << 4) | nibble(pitch_b);
            send_report(build_report(
                buzzer,
                pitch,
                LED_KEEP_PAIR,
                LED_KEEP_PAIR,
                LED_KEEP_HIGH,
            ))
        }
        Command::Reset => send_report(build_report(
            BUZZER_OFF, PITCH_OFF, LED_OFF, LED_OFF, LED_OFF,
        )),
        Command::Report { bytes } => {
            if bytes.len() != REPORT_LEN {
                return Err(ControlError::InvalidArg("report must be 8 bytes".into()));
            }
            let mut report = [0u8; REPORT_LEN];
            report.copy_from_slice(&bytes);
            send_report(report)
        }
    }
}

fn send_report(report: [u8; REPORT_LEN]) -> ControlResult<()> {
    let context = rusb::Context::new()?;
    let mut handle = context
        .open_device_with_vid_pid(VENDOR_ID, PRODUCT_ID)
        .ok_or(ControlError::DeviceNotFound)?;

    let _ = handle.set_auto_detach_kernel_driver(true);
    claim_interface_with_retry(&mut handle, 0)?;

    let timeout = Duration::from_millis(TIMEOUT_MS);
    let written = handle.write_interrupt(ENDPOINT_OUT, &report, timeout)?;
    if written != REPORT_LEN {
        return Err(ControlError::ShortWrite);
    }

    Ok(())
}

fn assemble_leds(color: u8, state: u8) -> ControlResult<(u8, u8, u8)> {
    let state = nibble(state);
    let keep = LED_KEEP_PAIR;
    match color {
        COLOR_RED => Ok(((state << 4) | LED_KEEP, keep, LED_KEEP_HIGH)),
        COLOR_YELLOW => Ok(((LED_KEEP << 4) | state, keep, LED_KEEP_HIGH)),
        COLOR_GREEN => Ok((keep, (state << 4) | LED_KEEP, LED_KEEP_HIGH)),
        COLOR_BLUE => Ok((keep, (LED_KEEP << 4) | state, LED_KEEP_HIGH)),
        COLOR_WHITE => Ok((keep, keep, state << 4)),
        _ => Err(ControlError::InvalidArg("color out of range".into())),
    }
}

fn build_report(buzzer: u8, pitch: u8, led_ry: u8, led_gb: u8, led_w: u8) -> [u8; REPORT_LEN] {
    [
        COMMAND_VERSION,
        COMMAND_ID,
        buzzer,
        pitch,
        led_ry,
        led_gb,
        led_w,
        0,
    ]
}

fn claim_interface_with_retry<T: UsbContext>(
    handle: &mut rusb::DeviceHandle<T>,
    interface: u8,
) -> ControlResult<()> {
    let strategy = Fixed::from_millis(BUSY_RETRY_DELAY_MS).take(BUSY_RETRY_ATTEMPTS);
    retry(strategy, || match handle.claim_interface(interface) {
        Ok(()) => OperationResult::Ok(()),
        Err(rusb::Error::Busy) => OperationResult::Retry(rusb::Error::Busy),
        Err(err) => OperationResult::Err(err),
    })
    .map_err(|err| ControlError::from(err.error))
}

fn nibble(value: u8) -> u8 {
    value & 0x0F
}

#[derive(Debug)]
enum ControlError {
    DeviceNotFound,
    InvalidArg(String),
    Usb(rusb::Error),
    ShortWrite,
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlError::DeviceNotFound => write!(f, "device 191a:8003 not found"),
            ControlError::InvalidArg(msg) => write!(f, "{msg}"),
            ControlError::Usb(err) => write!(f, "usb error: {err}"),
            ControlError::ShortWrite => write!(f, "usb short write"),
        }
    }
}

impl From<rusb::Error> for ControlError {
    fn from(err: rusb::Error) -> Self {
        ControlError::Usb(err)
    }
}

fn parse_color(value: &str) -> Result<u8, String> {
    if let Some(alias) = color_alias(value) {
        return Ok(alias);
    }

    let num = parse_u8_any(value).map_err(|_| format!("unknown color '{value}'"))?;
    if num <= COLOR_WHITE {
        Ok(num)
    } else {
        Err(format!(
            "color index {num} out of range (0-{})",
            COLOR_WHITE
        ))
    }
}

fn color_alias(value: &str) -> Option<u8> {
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "red" => Some(COLOR_RED),
        "yellow" => Some(COLOR_YELLOW),
        "green" => Some(COLOR_GREEN),
        "blue" => Some(COLOR_BLUE),
        "white" => Some(COLOR_WHITE),
        _ => None,
    }
}

fn parse_led_state(value: &str) -> Result<u8, String> {
    if let Some(alias) = led_state_alias(value) {
        return Ok(alias);
    }
    parse_nibble(value)
}

fn led_state_alias(value: &str) -> Option<u8> {
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "led_off" | "off" => Some(0x0),
        "led_on" | "on" | "solid" => Some(0x1),
        "led_pattern1" | "pattern1" => Some(0x2),
        "led_pattern2" | "pattern2" => Some(0x3),
        "led_pattern3" | "pattern3" => Some(0x4),
        "led_pattern4" | "pattern4" => Some(0x5),
        "led_keep" | "keep" => Some(LED_KEEP),
        _ => None,
    }
}

fn parse_buzzer_pattern(value: &str) -> Result<u8, String> {
    if let Some(alias) = buzzer_alias(value) {
        return Ok(alias);
    }
    parse_nibble(value)
}

fn buzzer_alias(value: &str) -> Option<u8> {
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "buzz_off" | "buzzer_off" | "off" => Some(0x0),
        "buzz_on" | "buzzer_on" | "on" => Some(0x1),
        "buzz_pattern1" | "pattern1" => Some(0x2),
        "buzz_pattern2" | "pattern2" => Some(0x3),
        "buzz_pattern3" | "pattern3" => Some(0x4),
        "buzz_pattern4" | "pattern4" => Some(0x5),
        "buzzer_keep" | "keep" => Some(LED_KEEP),
        _ => None,
    }
}

fn parse_nibble(value: &str) -> Result<u8, String> {
    let num = parse_u8_any(value).map_err(|_| format!("invalid nibble '{value}'"))?;
    if num <= 0x0F {
        Ok(num)
    } else {
        Err(format!("nibble {num:#x} out of range (0x0-0xF)"))
    }
}

fn parse_byte(value: &str) -> Result<u8, String> {
    parse_u8_any(value).map_err(|_| format!("invalid byte '{value}'"))
}

fn parse_u8_any(value: &str) -> Result<u8, String> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u8::from_str_radix(hex, 16).map_err(|_| format!("invalid hex value '{value}'"))
    } else {
        value
            .parse::<u8>()
            .map_err(|_| format!("invalid number '{value}'"))
    }
}
