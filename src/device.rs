use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_hid::{AsyncHidWrite, DeviceReader, DeviceWriter, HidBackend};
use byteorder::{BigEndian, LittleEndian, WriteBytesExt};
use rand::rngs::ThreadRng;
use rand::seq::SliceRandom;
use rand::{RngExt, distr::Alphanumeric};
use data_url::DataUrl;
use futures_util::StreamExt;
use log::{debug, info, warn};
use serde_json::json;
use tokio::sync::Mutex as TokioMutex;
use tokio::time::Duration;
use zip::write::FileOptions;

use uuid::Uuid;

use image::{DynamicImage, GenericImageView, RgbImage, RgbaImage};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const VENDOR_ID: u16 = 0x2207;
pub const PRODUCT_ID: u16 = 0x0019;
pub const DEVICE_NAMESPACE: &str = "e9";

const PACKET_SIZE: usize = 1024;
const HEADER: [u8; 2] = [0x7c, 0x7c];
const USAGE_PAGE: u16 = 0x000c;
// All three hardware variants use the same HID VID/PID and the same 5x3
// image bundle protocol.
//
// Grid slots (OpenDeck keypad position == hardware screen index):
//   0..=12  the single LCD keys
//   13      the double-width ("wide") screen, the one the status window
//           command (OutSetSmallWindowData) drives
//   14      phantom: the right half of slot 13, never addressable
//
// The D200/D200H have 14 physical keys, so slot 14 is a ghost cell there.
pub const NUM_BUTTONS: usize = 15;
/// Slot holding the double-width status screen.
pub const WIDE_KEY: usize = 13;
/// Right half of the wide screen: not addressable, left out of the bundle.
pub const PHANTOM_KEY: usize = 14;
/// Native icon resolution of a single screen.
pub const ICON_SIZE: u32 = 196;
/// The firmware squeezes a wide icon from 392 -> 196, so compose it on a 2:1
/// canvas of this width.
pub const WIDE_ICON_WIDTH: u32 = 392;
/// Hardware input indexes reporting the three rotary encoders (0, 1, 2).
pub const DIAL_BASE: usize = 17;
pub const NUM_ENCODERS: usize = 3;
/// D200X side buttons are reported inside the same index space as the dials.
/// Hardware indexes for the two side buttons (touchpoints).
pub const SIDE_BUTTON_0: usize = 15;
pub const SIDE_BUTTON_1: usize = 16;
/// Byte 10 of an encoder input report carries the dial event itself:
/// 0 = released, 1 = pressed, 2 = turned counterclockwise, 3 = turned clockwise.
const DIAL_RELEASE: u8 = 0;
const DIAL_PRESS: u8 = 1;
const DIAL_TURN_LEFT: u8 = 2;
const DIAL_TURN_RIGHT: u8 = 3;
const MAX_INPUT_INDEX: usize = 19;

const MAX_COMMAND_PAYLOAD: usize = PACKET_SIZE - 8; // 1016

/// Pacing for the icon transfer.
///
/// CHANGELOG 0.6.1 attributes screen blinking to the device receiving packets
/// "too many packets too fast", so the transfer is paced in small bursts.
///
/// The defaults were chosen from measurements on real D200 hardware with a
/// 156 kB / 153-packet bundle (the size the plugin actually sends):
///
///   no pacing .................... 40 ms
///   burst 8, 2 ms ................ 90 ms   (worst: costs +50 ms)
///   burst 32, 2 ms ............... 48 ms
///   burst 64, 1 ms ............... 41 ms   (default: essentially free)
///   burst 128, 1 ms .............. 41 ms
///
/// So the default is deliberately loose: it still breaks the stream up, but
/// costs about 1 ms instead of 50 ms. Note this pacing has *not* been shown to
/// prevent blinking on this firmware - that remains unverified, since blinking
/// cannot be observed programmatically.
///
/// Both values can be overridden at runtime, no rebuild needed:
///
///   ULANZI_BURST=0     disable pacing entirely
///   ULANZI_BURST=8 ULANZI_PACKET_MS=2    stricter pacing, at a latency cost
fn burst_size() -> usize {
    std::env::var("ULANZI_BURST")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64)
}

fn packet_delay() -> Duration {
    let ms = std::env::var("ULANZI_PACKET_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1u64);
    Duration::from_millis(ms)
}

// ---------------------------------------------------------------------------
// Command protocol
// ---------------------------------------------------------------------------

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CommandProtocol {
    OutSetButtons = 0x0001,
    OutSetSmallWindowData = 0x0006,
    OutSetBrightness = 0x000a,
    OutSetLabelStyle = 0x000b,
    InButton = 0x0101,
    InButton2 = 0x0102,
}

// ---------------------------------------------------------------------------
// Button event
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct ButtonEvent {
    pub index: usize,
    pub pressed: bool,
    pub state: u8,
}

/// The D200X multiplexes its dials and side buttons into the same report
/// stream as the LCD keys. The state byte disambiguates them: 2 means a dial
/// turn, while a plain press/release carries 0 or 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// An LCD key (0..=12, or 13/14 on the wide screen).
    Key { index: usize, pressed: bool },
    /// A rotary encoder: `position` 0..2, `ticks` -1 for left, +1 for right.
    Encoder { position: u8, ticks: i16 },
    /// An encoder push.
    EncoderPress { position: u8, pressed: bool },
    /// A side button (no display of its own).
    SideButton { index: usize, pressed: bool },
}

// ---------------------------------------------------------------------------
// Button data structure
// ---------------------------------------------------------------------------

#[derive(Clone)] // if you need cloning
pub struct ButtonImageData {
    pub image: Vec<u8>,
    pub uuid: Uuid,
    /// Fingerprint of the payload OpenDeck sent for this slot, used to skip
    /// re-encoding an icon that is already on screen.
    pub source_hash: u64,
}

/// FNV-1a: cheap, non-cryptographic digest for change detection only.
fn short_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

// ---------------------------------------------------------------------------
// Device handle
// ---------------------------------------------------------------------------

pub struct UlanziDevice {
    writer: Arc<TokioMutex<DeviceWriter>>,
    reader: Option<DeviceReader>,
    id: String,
    button_images: Mutex<HashMap<usize, ButtonImageData>>,
    dirty_buttons: Mutex<std::collections::HashSet<usize>>,
}

// ---------------------------------------------------------------------------
// Aspect‑ratio‑preserving resize helper
// ---------------------------------------------------------------------------

/// Resize an image to fit inside `size`×`size`, preserving aspect ratio.
/// The padding is transparent if the source has an alpha channel,
/// otherwise opaque black.
fn resize_square(img: &DynamicImage, size: u32) -> DynamicImage {
    let (w, h) = img.dimensions();
    if w == size && h == size {
        return img.clone();
    }

    // Scale factor to fit entirely inside the square
    let scale = (size as f64 / w as f64).min(size as f64 / h as f64);
    let new_w = (w as f64 * scale).round() as u32;
    let new_h = (h as f64 * scale).round() as u32;

    // Resize to the new dimensions (keeps colour type)
    let resized = img.resize(new_w, new_h, image::imageops::FilterType::Triangle);

    // Determine if the source had alpha (RGBA8, LA8, etc.)
    let has_alpha = matches!(
        img.color(),
        image::ColorType::Rgba8 | image::ColorType::La8 | image::ColorType::Rgba16 | image::ColorType::La16
    );

    let x = ((size - new_w) / 2) as i64;
    let y = ((size - new_h) / 2) as i64;

    if has_alpha {
        // Transparent canvas
        let mut canvas = RgbaImage::from_pixel(size, size, image::Rgba([0, 0, 0, 0]));
        image::imageops::overlay(&mut canvas, &resized.to_rgba8(), x, y);
        DynamicImage::ImageRgba8(canvas)
    } else {
        // Opaque black canvas
        let mut canvas = RgbImage::from_pixel(size, size, image::Rgb([0, 0, 0]));
        let rgb = resized.to_rgb8();
        image::imageops::overlay(&mut canvas, &rgb, x, y);
        DynamicImage::ImageRgb8(canvas)
    }
}

/// Render the D200X wide key without stretching its source before the
/// firmware's own 2:1 display conversion.
fn resize_wide(img: &DynamicImage) -> DynamicImage {
    let (w, h) = img.dimensions();
    let scale = (WIDE_ICON_WIDTH as f64 / w as f64).min(ICON_SIZE as f64 / h as f64);
    let nw = (w as f64 * scale).round() as u32;
    let nh = (h as f64 * scale).round() as u32;
    let resized = img.resize(nw, nh, image::imageops::FilterType::Triangle).to_rgb8();
    let mut canvas = RgbImage::from_pixel(WIDE_ICON_WIDTH, ICON_SIZE, image::Rgb([0, 0, 0]));
    image::imageops::overlay(&mut canvas, &resized, ((WIDE_ICON_WIDTH - nw) / 2) as i64, ((ICON_SIZE - nh) / 2) as i64);
    DynamicImage::ImageRgb8(canvas)
}

impl UlanziDevice {
    // -- Construction -------------------------------------------------------

    pub async fn connect() -> Result<Self> {
        let backend = HidBackend::default();
        let devices: Vec<_> = backend.enumerate().await?.collect().await;

        let device_info = devices
            .into_iter()
            .find(|d| {
                d.vendor_id == VENDOR_ID
                    && d.product_id == PRODUCT_ID
                    && d.usage_page == USAGE_PAGE
            })
            .ok_or_else(|| anyhow!("Ulanzi D200 device not found"))?;

        // The HID identifiers are shared by the D200/H and D200X. The richer
        // report indexes identify the D200X, but until a first report arrives
        // accept grid writes for the family and consistently emit the wide
        // aspect image; on older models only the screen's fixed aspect differs.

        let (reader, writer) = device_info.open().await?;

        let id = Self::generate_id(
            device_info.serial_number.as_deref(),
            &format!("{:?}", device_info.id),
        );

        info!("Connected to Ulanzi D200 (ID: {})", id);

        Ok(Self {
            writer: Arc::new(TokioMutex::new(writer)),
            reader: Some(reader),
            id,
            button_images: Mutex::new(HashMap::new()),
            dirty_buttons: Mutex::new(std::collections::HashSet::new()),
        })
    }

    fn generate_id(serial: Option<&str>, fallback: &str) -> String {
        match serial {
            Some(s) => format!("{}-{}", DEVICE_NAMESPACE, s),
            None => format!("{}-{}", DEVICE_NAMESPACE, fallback),
        }
    }

    // -- Accessors ----------------------------------------------------------

    pub fn get_id(&self) -> &str {
        &self.id
    }

    pub fn take_reader(&mut self) -> Option<DeviceReader> {
        self.reader.take()
    }

    /// True while the device still owns an input reader task. Used by the
    /// daemon to decide whether a freshly enumerated device needs setup.
    pub fn has_reader(&self) -> bool {
        self.reader.is_some()
    }

    // -- Report parsing -----------------------------------------------------

    pub fn parse_report(buf: &[u8]) -> Option<ButtonEvent> {
        if buf.len() < 12 || buf[0..2] != HEADER {
            return None;
        }

        let command = u16::from_be_bytes([buf[2], buf[3]]);
        if command != CommandProtocol::InButton as u16
            && command != CommandProtocol::InButton2 as u16
        {
            return None;
        }

        let index = buf[9] as usize;
        if index > MAX_INPUT_INDEX {
            warn!("Received button event with out-of-range index {}", index);
            return None;
        }

        Some(ButtonEvent {
            state: buf[8],
            index,
            pressed: buf[11] == 0x01,
        })
    }

    pub fn parse_input(buf: &[u8]) -> Option<InputEvent> {
        let raw = Self::parse_report(buf)?;

        // Side buttons (touchpoints) are hardware indexes 15 and 16, which sit
        // directly after the 15 keypad slots. They are checked first so they
        // are never misclassified as an encoder even if the firmware sets the
        // rotation marker on them.
        if raw.index == SIDE_BUTTON_0 || raw.index == SIDE_BUTTON_1 {
            return Some(InputEvent::SideButton {
                index: raw.index,
                pressed: raw.pressed,
            });
        }

        // The three rotary encoders report as indexes 17, 18 and 19.
        //
        // Firmware layout (matching the reference implementation):
        //   byte 10 (body[2]) == 2 marks the report as encoder traffic
        //   byte 11 (body[3]) is the dial event:
        //     0 = release, 1 = press, 2 = turn left, 3 = turn right
        let in_dial_range = (DIAL_BASE..DIAL_BASE + NUM_ENCODERS).contains(&raw.index);
        if in_dial_range {
            let position = (raw.index - DIAL_BASE) as u8;
            let dial_event = buf[11];
            debug!(
                "Encoder raw: dial={} state={} b10={} b11={}",
                position, raw.state, buf[10], dial_event
            );
            match dial_event {
                DIAL_TURN_LEFT | DIAL_TURN_RIGHT => {
                    // The reference maps 2 -> -1 (left) and 3 -> +1 (right).
                    // Some D200X units wire the encoder the other way round, so
                    // ULANZI_INVERT_DIAL=1 flips it without a rebuild.
                    let mut ticks = if dial_event == DIAL_TURN_RIGHT { 1 } else { -1 };
                    if std::env::var("ULANZI_INVERT_DIAL")
                        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                        .unwrap_or(false)
                    {
                        ticks = -ticks;
                    }
                    return Some(InputEvent::Encoder { position, ticks });
                }
                DIAL_PRESS => {
                    return Some(InputEvent::EncoderPress { position, pressed: true });
                }
                DIAL_RELEASE => {
                    return Some(InputEvent::EncoderPress { position, pressed: false });
                }
                other => {
                    debug!("Unknown encoder event {other} from dial {position}");
                    return None;
                }
            }
        }

        // The right half of the double-width screen is not addressable.
        if raw.index == PHANTOM_KEY {
            return None;
        }

        Some(InputEvent::Key {
            index: raw.index,
            pressed: raw.pressed,
        })
    }


    // -- High‑level commands ------------------------------------------------

    pub async fn set_small_window_data(
        &self,
        mode: u8,
        cpu: u8,
        mem: u8,
        time_str: &str,
        gpu: u8,
    ) -> Result<()> {
        let payload = format!("{}|{}|{}|{}|{}", mode, cpu, mem, time_str, gpu).into_bytes();
        self.send_command(CommandProtocol::OutSetSmallWindowData, &payload)
            .await
    }

    pub async fn set_brightness(&self, brightness: u8) -> Result<()> {
        let brightness = brightness.min(100);
        let payload = brightness.to_string().into_bytes();
        self.send_command(CommandProtocol::OutSetBrightness, &payload)
            .await?;
        debug!("Set brightness to {}%", brightness);
        Ok(())
    }

    pub async fn set_label_style(&self, style: &serde_json::Value) -> Result<()> {
        let payload = serde_json::to_vec(style)?;
        self.send_command(CommandProtocol::OutSetLabelStyle, &payload)
            .await?;
        debug!("Set label style");
        Ok(())
    }



    /// Stage a button image from a data URL (Base64) or a file path.
    /// Returns `Ok(true)` if the image was new/different, `Ok(false)` if unchanged.
    /// Call `flush()` to apply all staged images.
    pub async fn set_button_image(&self, index: usize, image_data: &str) -> Result<bool> {
        // Touchpoints 15/16 are the round side buttons: they have no display, so
        // OpenDeck still sends us an image for them but there is nothing to draw.
        if index == SIDE_BUTTON_0 || index == SIDE_BUTTON_1 {
            debug!("Ignoring image for display-less touchpoint {}", index);
            return Ok(false);
        }
        if index == PHANTOM_KEY {
            return Err(anyhow!("Button index {} is the phantom half of the wide screen", index));
        }
        if index >= NUM_BUTTONS {
            return Err(anyhow!(
                "Button index {} out of range (0..{})",
                index,
                NUM_BUTTONS - 1
            ));
        }

        // Cheap fingerprint of the *source* payload. Scene switches often
        // re-send the same icon for untouched keys; comparing the raw input
        // lets us skip decoding, resizing and PNG-encoding entirely, which is
        // by far the most expensive part of a page change.
        let fingerprint = short_hash(image_data.as_bytes());

        {
            let map = self.button_images.lock().unwrap();
            if let Some(existing) = map.get(&index) {
                if existing.source_hash == fingerprint {
                    return Ok(false);
                }
            }
        }

        // Decoding, resizing and PNG-encoding 14 icons costs tens of
        // milliseconds of pure CPU. Running that inline on the tokio worker
        // blocks the daemon loop, delaying the flush and every other event
        // that arrives meanwhile, so move it to the blocking pool.
        let is_data_url = image_data.starts_with("data:");
        let owned = image_data.to_string();
        let png_data = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let img = if is_data_url {
                let url = DataUrl::process(&owned).map_err(|_| anyhow!("Invalid data URL"))?;
                let (body, _) = url
                    .decode_to_vec()
                    .map_err(|_| anyhow!("Failed to decode data URL"))?;
                image::load_from_memory(&body)?
            } else {
                let path = std::path::Path::new(&owned);
                if !path.exists() {
                    return Err(anyhow!("Image file not found: {}", owned));
                }
                image::open(path)
                    .map_err(|e| anyhow!("Failed to open image {}: {}", owned, e))?
            };

            let resized = if index == WIDE_KEY {
                resize_wide(&img)
            } else {
                resize_square(&img, ICON_SIZE)
            };
            let mut buf = Vec::new();
            {
                let mut cursor = Cursor::new(&mut buf);
                resized.write_to(&mut cursor, image::ImageFormat::Png)?;
            }
            Ok(buf)
        })
        .await
        .map_err(|e| anyhow!("Image processing task failed: {}", e))??;

        let mut map = self.button_images.lock().unwrap();
        let changed = match map.get(&index) {
            Some(existing) => existing.image != png_data,
            None => true,
        };

        if changed {
            map.insert(
                index,
                ButtonImageData {
                    image: png_data,
                    uuid: Uuid::now_v7(),
                    source_hash: fingerprint,
                },
            );
            self.dirty_buttons.lock().unwrap().insert(index);
        }

        Ok(changed)
    }

    /// Remove a staged button image (will be cleared on next `flush()`).
    pub fn clear_button_image(&self, index: usize) {
        if index == SIDE_BUTTON_0 || index == SIDE_BUTTON_1 || index == PHANTOM_KEY {
            return;
        }
        if index >= NUM_BUTTONS {
            warn!("Attempt to clear out‑of‑range button index {}", index);
            return;
        }
        let mut map = self.button_images.lock().unwrap();
        if map.remove(&index).is_some() {
            self.dirty_buttons.lock().unwrap().insert(index);
        }
    }

    /// Remove **all** staged button images and send an empty configuration
    /// to the device (clears all buttons).
    #[allow(dead_code)]
    pub async fn clear_all_images(&self) -> Result<()> {
        self.forget_all_images();
        self.flush().await
    }

    /// Drop every staged image and mark the grid dirty **without** flushing.
    ///
    /// Used when (re)connecting: pushing an all-empty bundle to a freshly
    /// initialised deck blanks every screen, and the replacement icons only
    /// arrive once OpenDeck pushes them. The result is a visible flash of
    /// black keys - exactly the "tela apagada" symptom. Clearing locally and
    /// letting the first real image batch paint avoids that flash entirely.
    pub fn forget_all_images(&self) {
        {
            let mut map = self.button_images.lock().unwrap();
            map.clear();
        }
        let mut dirty = self.dirty_buttons.lock().unwrap();
        for i in 0..NUM_BUTTONS {
            if i != PHANTOM_KEY && i != SIDE_BUTTON_0 && i != SIDE_BUTTON_1 {
                dirty.insert(i);
            }
        }
    }

    /// Send the currently staged button images to the device.
    /// Uses unique filenames per flush to force device to reload icons.
    /// Bounded retries – returns error if a valid ZIP cannot be built.
    pub async fn flush(&self) -> Result<()> {
        debug!("Building button configuration ZIP");

        // Claim the pending set now, but put it back if the transfer fails.
        // Dropping it on error would leave those keys permanently stuck with
        // whatever the device last received, i.e. a stale or blank screen that
        // never recovers because no later flush considers them dirty.
        let claimed = {
            let mut d = self.dirty_buttons.lock().unwrap();
            if d.is_empty() {
                debug!("No dirty buttons, skipping flush");
                return Ok(());
            }
            std::mem::take(&mut *d)
        };
        debug!("Flushing {} dirty button(s)", claimed.len());

        let result = self.build_and_send_bundle().await;
        if result.is_err() {
            let mut d = self.dirty_buttons.lock().unwrap();
            d.extend(claimed);
        }
        result
    }

    /// First offset >= `first` (stepping by `step`) that holds a byte the
    /// firmware chokes on, if any.
    fn first_bad_offset(data: &[u8], first: usize, step: usize) -> Option<usize> {
        (first..data.len())
            .step_by(step)
            .find(|&o| matches!(data.get(o), Some(0x00) | Some(0x7c)))
    }

    /// Assemble the icon ZIP.
    ///
    /// `pad` adds a bounded amount of incompressible filler used to shift where
    /// bytes land, and `attempt` drives the entry ordering, so consecutive
    /// calls produce a different archive of essentially the same size.
    fn build_bundle(
        images_snapshot: &HashMap<usize, ButtonImageData>,
        pad: usize,
        attempt: usize,
    ) -> Result<Vec<u8>> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            let deflated = FileOptions::<()>::default()
                .compression_method(zip::CompressionMethod::Deflated);

            if pad > 0 {
                // Random filler: a repeating pattern compresses to nothing and
                // would not move the bytes that matter.
                let filler: String = ThreadRng::default()
                    .sample_iter(&Alphanumeric)
                    .take(pad)
                    .map(char::from)
                    .collect();
                zip.start_file("pad.bin", deflated)?;
                zip.write_all(filler.as_bytes())?;
            }

            let mut manifest = json!({});

            // Shuffling the write order varies where each image's compressed
            // bytes land, so a bundle that already tripped the bug does not
            // reproduce it identically on the next attempt.
            let mut numbers: Vec<usize> = (0..NUM_BUTTONS).collect();
            numbers.shuffle(&mut ThreadRng::default());
            if attempt % 2 == 1 {
                numbers.rotate_left(1);
            }

            for (index, value) in numbers.into_iter().enumerate() {
                if value == PHANTOM_KEY {
                    continue;
                }
                let col = value % 5;
                let row = value / 5;
                let key = format!("{}_{}", col, row);
                let mut view_param = json!({ "Text": "" });

                if let Some(img_data) = images_snapshot.get(&value) {
                    let icon_name = format!("{}_{}.png", index, img_data.uuid);
                    zip.start_file(format!("Images/{}", icon_name), deflated)?;
                    zip.write_all(&img_data.image)?;
                    view_param["Icon"] = json!(format!("Images/{}", icon_name));
                } else {
                    view_param["Icon"] = json!("");
                }

                manifest[key] = json!({ "State": 0, "ViewParam": [view_param] });
            }

            zip.start_file("manifest.json", deflated)?;
            zip.write_all(serde_json::to_string(&manifest)?.as_bytes())?;

            zip.start_file("sentinel.txt", deflated)?;
            zip.write_all(b"")?;

            zip.finish()?;
        }
        Ok(cursor.into_inner())
    }

    /// Build the icon bundle (applying the hardware byte-offset workaround) and
    /// push it to the device.
    async fn build_and_send_bundle(&self) -> Result<()> {
        let images_snapshot = {
            let map = self.button_images.lock().unwrap();
            map.clone()
        };
        // Hardware bug workaround (see CHANGELOG 0.5.0/0.6.0): the firmware
        // rejects the uploaded bundle when specific byte values land at the
        // start of any packet, so the archive is rebuilt until every packet
        // boundary is clean.
        //
        // Critically, the size of the bundle must NOT grow between attempts.
        // The original implementation padded a dummy file by 1024*retries
        // bytes, which added one more checked offset per retry and made the
        // search diverge: at ~800 checked offsets the chance of a clean bundle
        // is already 0.2%, at ~1600 it is effectively zero. Any user with rich
        // artwork (a ~1.3 MB bundle) hit "Failed to build a valid ZIP after
        // 1000 retries" and never painted a screen at all.
        //
        // Instead we keep the bundle byte-identical in size and only reshuffle
        // the order the entries are written in, plus vary a small amount of
        // in-place padding. That moves the compressed bytes around without
        // changing how many offsets have to be checked.
        const MAX_RETRIES: usize = 64;
        const OFFSET_STEP: usize = 1024;
        // Small, fixed-size slack so the archive can be nudged without ever
        // adding a new checked offset.
        const PAD_SLACK: usize = 512;
        // Wall-clock ceiling for the retry hunt.
        const RETRY_TIME_BUDGET: std::time::Duration = std::time::Duration::from_millis(600);

        // The header packet carries the first 1016 bytes of the ZIP, so every
        // following packet starts at 1016 + n * 1024. The firmware rejects the
        // bundle if *any* of those packets starts with an invalid byte, so the
        // whole archive has to be checked, not just its tail.
        const FIRST_PACKET_BOUNDARY: usize = 1016;

        // Rebuilding and re-checking a large archive is not free, and for a
        // big bundle the odds of finding a byte-clean one are low (every
        // packet boundary has to pass). Cap the time spent hunting so a hard
        // scene can never turn into a multi-minute freeze, and fall back to the
        // best attempt found so far.
        let hunt_deadline = std::time::Instant::now() + RETRY_TIME_BUDGET;

        let mut retries = 0usize;
        let mut best: Option<(usize, Vec<u8>)> = None;
        let zip_data = loop {
            let pad = (retries % 64) * (PAD_SLACK / 64);
            let bundle = Self::build_bundle(&images_snapshot, pad, retries)?;

            match Self::first_bad_offset(&bundle, FIRST_PACKET_BOUNDARY, OFFSET_STEP) {
                None => break bundle,
                Some(offset) => {
                    // Keep the attempt that got furthest past the bad offset.
                    let better = match &best {
                        None => true,
                        Some((previous_offset, _)) => offset > *previous_offset,
                    };
                    if better {
                        best = Some((offset, bundle));
                    }
                    retries += 1;
                    if retries >= MAX_RETRIES || std::time::Instant::now() >= hunt_deadline {
                        // Sending a bundle that still trips the firmware bug is
                        // far better than giving up: not sending anything at all
                        // leaves the previous (possibly blank) screen in place
                        // and looks exactly like the "tela apagada" symptom.
                        // The firmware is the one that decides whether to accept
                        // it, so send the best attempt we have.
                        warn!(
                            "No byte-clean icon bundle after {} retries; sending the \
                             best attempt anyway so the screens still update",
                            MAX_RETRIES
                        );
                        break best
                            .expect("best attempt is always set once a retry happens")
                            .1;
                    }
                    debug!(
                        "Invalid byte at offset {} (retry {}, pad {})",
                        offset,
                        retries,
                        pad
                    );
                }
            }
        };
        let dummy_retries = retries;
        if dummy_retries > 0 {
            info!(
                "Icon bundle needed {} rebuild(s) to dodge the firmware byte bug ({} bytes)",
                dummy_retries,
                zip_data.len()
            );
        }

        let t = std::time::Instant::now();
        self.send_file(&zip_data).await?;
        info!(
            "Sent button configuration: {} bytes, {} packets, transferred in {:?} ({} retries)",
            zip_data.len(),
            zip_data.len() / 1024 + 1,
            t.elapsed(),
            dummy_retries
        );
        Ok(())
    }

    // -- Low‑level packet I/O -----------------------------------------------

    async fn send_command(&self, command: CommandProtocol, payload: &[u8]) -> Result<()> {
        if payload.len() > MAX_COMMAND_PAYLOAD {
            return Err(anyhow!(
                "Command payload too large: {} bytes (max {})",
                payload.len(),
                MAX_COMMAND_PAYLOAD
            ));
        }
        let packet = self.build_packet(command, payload, payload.len() as u32);
        self.writer.lock().await.write_output_report(&packet).await?;
        Ok(())
    }

    async fn send_file(&self, data: &[u8]) -> Result<()> {
        let file_size = data.len() as u32;
        debug!("Sending icon data! ({} bytes)", file_size);
        let first_chunk = if data.len() >= 1016 {
            &data[..1016]
        } else {
            data
        };
        let first_packet =
            self.build_packet(CommandProtocol::OutSetButtons, first_chunk, file_size);

        let mut writer = self.writer.lock().await;
        writer.write_output_report(&first_packet).await?;

        let mut chunks_in_burst = 0usize;
        let burst = burst_size();
        let delay = packet_delay();
        if data.len() > 1016 {
            for chunk in data[1016..].chunks(1024) {
                let mut packet = [0u8; PACKET_SIZE];
                let len = chunk.len().min(PACKET_SIZE);
                packet[..len].copy_from_slice(&chunk[..len]);
                writer.write_output_report(&packet).await?;

                // The firmware drops or truncates the bundle when the host
                // streams packets back to back (CHANGELOG 0.6.1: "blinking
                // caused by the device receiving too many packets too fast").
                // A short pause every few packets keeps its HID buffer drained
                // so the screens settle instead of flashing blank.
                chunks_in_burst += 1;
                if burst >= 1 && chunks_in_burst >= burst {
                    chunks_in_burst = 0;
                    tokio::time::sleep(delay).await;
                }
            }
        }
        // Let the device drain the tail of the stream before the next
        // command arrives, so the firmware can finish committing the bundle.
        if burst >= 1 && chunks_in_burst > 0 {
            tokio::time::sleep(delay).await;
        }
        Ok(())
    }

    fn build_packet(&self, command: CommandProtocol, data: &[u8], total_length: u32) -> Vec<u8> {
        let mut packet = Vec::with_capacity(PACKET_SIZE);
        packet.extend_from_slice(&HEADER);

        let mut cmd_buf = [0u8; 2];
        (&mut cmd_buf[..])
            .write_u16::<BigEndian>(command as u16)
            .unwrap();
        packet.extend_from_slice(&cmd_buf);

        // Device expects total_length as little‑endian (working protocol).
        let mut len_buf = [0u8; 4];
        (&mut len_buf[..])
            .write_u32::<LittleEndian>(total_length)
            .unwrap();
        packet.extend_from_slice(&len_buf);

        let data_len = data.len().min(PACKET_SIZE - 8);
        packet.extend_from_slice(&data[..data_len]);

        if packet.len() < PACKET_SIZE {
            packet.resize(PACKET_SIZE, 0);
        }
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_id_with_serial() {
        let id = UlanziDevice::generate_id(Some("1234567890"), "fallback");
        assert_eq!(id, "e9-1234567890");
    }

    #[test]
    fn test_generate_id_without_serial() {
        let id = UlanziDevice::generate_id(None, "fallback");
        assert_eq!(id, "e9-fallback");
    }

    /// Build a synthetic 12-byte input report the way the firmware does:
    /// `7c 7c`, big-endian command, 4-byte length, then state/index/marker/payload.
    fn report(command: u16, state: u8, index: u8, byte10: u8, byte11: u8) -> Vec<u8> {
        let mut buf = vec![0u8; 12];
        buf[0] = 0x7c;
        buf[1] = 0x7c;
        buf[2..4].copy_from_slice(&command.to_be_bytes());
        buf[8] = state;
        buf[9] = index;
        buf[10] = byte10;
        buf[11] = byte11;
        buf
    }

    #[test]
    fn test_parse_input_regular_key() {
        let buf = report(0x0101, 1, 5, 0, 1);
        match UlanziDevice::parse_input(&buf) {
            Some(InputEvent::Key { index, pressed }) => {
                assert_eq!(index, 5);
                assert!(pressed);
            }
            other => panic!("expected Key, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_input_phantom_key_is_dropped() {
        let buf = report(0x0101, 1, PHANTOM_KEY as u8, 0, 1);
        assert!(UlanziDevice::parse_input(&buf).is_none());
    }

    #[test]
    fn test_parse_input_side_buttons() {
        for idx in [SIDE_BUTTON_0, SIDE_BUTTON_1] {
            let pressed = report(0x0101, 1, idx as u8, 0, 1);
            match UlanziDevice::parse_input(&pressed) {
                Some(InputEvent::SideButton { index, pressed }) => {
                    assert_eq!(index, idx);
                    assert!(pressed);
                }
                other => panic!("expected SideButton for {idx}, got {:?}", other),
            }

            let released = report(0x0101, 0, idx as u8, 0, 0);
            match UlanziDevice::parse_input(&released) {
                Some(InputEvent::SideButton { index, pressed }) => {
                    assert_eq!(index, idx);
                    assert!(!pressed);
                }
                other => panic!("expected SideButton release for {idx}, got {:?}", other),
            }
        }
    }

    #[test]
    fn test_parse_input_encoders_rotate_and_press() {
        // Firmware layout: byte 10 marks encoder traffic (== 2), byte 11 is
        // the dial event (0 = release, 1 = press, 2 = left, 3 = right).
        for (idx, expected_pos) in [(17u8, 0u8), (18, 1), (19, 2)] {
            let left = report(0x0101, 2, idx, 2, 2);
            match UlanziDevice::parse_input(&left) {
                Some(InputEvent::Encoder { position, ticks }) => {
                    assert_eq!(position, expected_pos);
                    assert_eq!(ticks, -1);
                }
                other => panic!("expected left rotation for {idx}, got {:?}", other),
            }

            let right = report(0x0101, 2, idx, 2, 3);
            match UlanziDevice::parse_input(&right) {
                Some(InputEvent::Encoder { position, ticks }) => {
                    assert_eq!(position, expected_pos);
                    assert_eq!(ticks, 1);
                }
                other => panic!("expected right rotation for {idx}, got {:?}", other),
            }

            // Press and release are both flagged by byte 10 == 2, with the
            // actual event in byte 11.
            let down = report(0x0101, 1, idx, 2, 1);
            match UlanziDevice::parse_input(&down) {
                Some(InputEvent::EncoderPress { position, pressed }) => {
                    assert_eq!(position, expected_pos);
                    assert!(pressed);
                }
                other => panic!("expected encoder press for {idx}, got {:?}", other),
            }

            let up = report(0x0101, 0, idx, 2, 0);
            match UlanziDevice::parse_input(&up) {
                Some(InputEvent::EncoderPress { position, pressed }) => {
                    assert_eq!(position, expected_pos);
                    assert!(!pressed);
                }
                other => panic!("expected encoder release for {idx}, got {:?}", other),
            }
        }
    }

    #[test]
    fn test_short_hash_is_stable_and_distinguishing() {
        // Same input must fingerprint identically so an unchanged icon is
        // never re-encoded.
        assert_eq!(short_hash(b"scene-icon-a"), short_hash(b"scene-icon-a"));
        // Different payloads must not collide, otherwise a scene switch would
        // be silently ignored and the old screen would stay on the deck.
        assert_ne!(short_hash(b"scene-icon-a"), short_hash(b"scene-icon-b"));
        assert_ne!(short_hash(b""), short_hash(b"\x00"));
    }

    /// The transfer is paced in bursts; a long bundle must still pause often
    /// enough that the firmware can absorb it instead of truncating it.
    #[test]
    fn test_packet_pacing_covers_long_bundles() {
        // The bundle sizes seen in the real plugin log peak around 158 kB,
        // i.e. ~155 packets. Check a generous upper bound as well.
        for packets in [155usize, 600] {
            let burst = 8usize;
            let mut chunks_in_burst = 0usize;
            let mut pauses = 0usize;
            for _ in 0..packets {
                chunks_in_burst += 1;
                if chunks_in_burst >= burst {
                    chunks_in_burst = 0;
                    pauses += 1;
                }
            }
            // At least one pause per burst, so no single burst grows unbounded.
            assert_eq!(pauses, packets / burst);
            assert!(pauses > 0);
        }
    }

    /// Hardware smoke test: pushes a full 14-icon scene to a real deck and
    /// reports where the time goes. Ignored by default because it needs the
    /// device attached and will visibly repaint the screens.
    ///
    /// Run with:  cargo test --release -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires an attached Ulanzi D200"]
    async fn test_real_device_scene_switch_timing() {
        use std::time::Instant;

        let device = UlanziDevice::connect().await.expect("deck not attached");

        // 14 distinct detailed icons, roughly the size OpenDeck delivers.
        let mut payloads = Vec::new();
        for i in 0..NUM_BUTTONS {
            if i == PHANTOM_KEY {
                continue;
            }
            let mut sd: u64 = 0x51ED_u64.wrapping_add(i as u64);
            let src = image::RgbaImage::from_fn(512, 512, |x, y| {
                sd = sd.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let n = (sd >> 33) as u8;
                let g = (x as f64 / 512.0 * 255.0) as u8;
                let h = (y as f64 / 512.0 * 255.0) as u8;
                let band = if (x / 48 + y / 48) % 2 == 0 { 90 } else { 0 };
                image::Rgba([n.wrapping_add(band), g, h, 255])
            });
            let mut png = Vec::new();
            image::DynamicImage::ImageRgba8(src)
                .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            payloads.push((i, format!("data:image/png;base64,{}", b64_encode(&png))));
        }

        // Stage: decode + resize + PNG encode for every slot.
        let t_stage = Instant::now();
        for (index, payload) in &payloads {
            device
                .set_button_image(*index, payload)
                .await
                .expect("set_button_image");
        }
        let stage = t_stage.elapsed();

        // Push the whole scene to the deck.
        let t_flush = Instant::now();
        device.flush().await.expect("flush");
        let flush = t_flush.elapsed();

        println!("--- real hardware scene switch ---");
        println!("stage 13 icons : {:?}", stage);
        println!("flush to deck  : {:?}", flush);
        println!("total          : {:?}", stage + flush);
    }

    fn b64_encode(data: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in data.chunks(3) {
            let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(T[(n >> 18) as usize & 63] as char);
            out.push(T[(n >> 12) as usize & 63] as char);
            out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
        }
        out
    }

    #[test]
    fn test_burst_and_delay_have_sane_defaults() {
        // Defaults must stay in the range measured on real hardware: loose
        // enough that pacing costs ~1 ms rather than ~50 ms.
        assert!((32..=128).contains(&burst_size()));
        assert!(packet_delay() <= Duration::from_millis(2));
    }

    #[test]
    fn test_parse_input_ignores_unknown_command() {
        let buf = report(0x0999, 1, 3, 0, 1);
        assert!(UlanziDevice::parse_input(&buf).is_none());
    }
}
