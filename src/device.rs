use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Result};
use async_hid::{AsyncHidWrite, DeviceReader, DeviceWriter, HidBackend};
use byteorder::{BigEndian, LittleEndian, WriteBytesExt};
use data_url::DataUrl;
use futures_util::StreamExt;
use log::{debug, info, warn};
use rand::{rngs, RngExt, distr::Alphanumeric};
use rand::seq::SliceRandom;
use serde_json::json;
use tokio::time::Duration;
use tokio::sync::Mutex as TokioMutex;
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
/// Byte 10 of an input report: 2 marks a rotary-encoder turn.
const DIAL_ROTATE_MARKER: u8 = 2;
const MAX_INPUT_INDEX: usize = 19;

const MAX_COMMAND_PAYLOAD: usize = PACKET_SIZE - 8; // 1016

static FLUSH_COUNTER: AtomicU64 = AtomicU64::new(0);

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
}

// ---------------------------------------------------------------------------
// Device handle
// ---------------------------------------------------------------------------

pub struct UlanziDevice {
    writer: Arc<TokioMutex<DeviceWriter>>,
    reader: Option<DeviceReader>,
    id: String,
    button_images: Mutex<HashMap<usize, ButtonImageData>>,
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

        // The three rotary encoders report as indexes 17, 18 and 19. A press
        // carries state 0/1, a turn carries the rotation marker in byte 10
        // (2 = left, 3 = right). The index range alone identifies them, so the
        // subtraction below cannot underflow.
        let in_dial_range = (DIAL_BASE..DIAL_BASE + NUM_ENCODERS).contains(&raw.index);
        if in_dial_range {
            let position = (raw.index - DIAL_BASE) as u8;
            if buf[10] == DIAL_ROTATE_MARKER {
                let ticks = if buf[11] == 3 { 1 } else { -1 };
                return Some(InputEvent::Encoder { position, ticks });
            }
            return Some(InputEvent::EncoderPress {
                position,
                pressed: raw.pressed,
            });
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
        if index >= NUM_BUTTONS {
            return Err(anyhow!(
                "Button index {} out of range (0..{})",
                index,
                NUM_BUTTONS - 1
            ));
        }
        if index == PHANTOM_KEY {
            return Err(anyhow!("Button index {} is the phantom half of the wide screen", index));
        }

        let png_data = if image_data.starts_with("data:") {
            // Data URL (Base64)
            let url = DataUrl::process(image_data).map_err(|_| anyhow!("Invalid data URL"))?;
            let (body, _) = url
                .decode_to_vec()
                .map_err(|_| anyhow!("Failed to decode data URL"))?;
            let img = image::load_from_memory(&body)?;
            let resized = if index == WIDE_KEY { resize_wide(&img) } else { resize_square(&img, ICON_SIZE) };
            let mut buf = Vec::new();
            {
                let mut cursor = Cursor::new(&mut buf);
                resized.write_to(&mut cursor, image::ImageFormat::Png)?;
            }
            buf
        } else {
            // File path
            let path = std::path::Path::new(image_data);
            if !path.exists() {
                return Err(anyhow!("Image file not found: {}", image_data));
            }
            let img = image::open(path)
                .map_err(|e| anyhow!("Failed to open image {}: {}", image_data, e))?;
            let resized = if index == WIDE_KEY { resize_wide(&img) } else { resize_square(&img, ICON_SIZE) };
            let mut buf = Vec::new();
            {
                let mut cursor = Cursor::new(&mut buf);
                resized.write_to(&mut cursor, image::ImageFormat::Png)?;
            }
            buf
        };

        let mut map = self.button_images.lock().unwrap();
        let changed = match map.get(&index) {
            Some(existing) => existing.image != png_data,
            None => true,
        };

        if changed {
            map.insert(index, ButtonImageData {
                image: png_data,
                uuid: Uuid::now_v7(),
            });
        }

        Ok(changed)
    }

    /// Remove a staged button image (will be cleared on next `flush()`).
    pub fn clear_button_image(&self, index: usize) {
        if index >= NUM_BUTTONS {
            warn!("Attempt to clear out‑of‑range button index {}", index);
            return;
        }
        self.button_images.lock().unwrap().remove(&index);
    }

    /// Remove **all** staged button images and send an empty configuration
    /// to the device (clears all buttons).
    pub async fn clear_all_images(&self) -> Result<()> {
        self.button_images.lock().unwrap().clear();
        self.flush().await
    }

    /// Send the currently staged button images to the device.
    /// Uses unique filenames per flush to force device to reload icons.
    /// Bounded retries – returns error if a valid ZIP cannot be built.
    pub async fn flush(&self) -> Result<()> {
        debug!("Building button configuration ZIP with bug workaround");

        let mut images_snapshot = {
            let map = self.button_images.lock().unwrap();
            map.clone()
        };

        const INVALID_BYTES: [u8; 2] = [0x00, 0x7c];
        const MAX_RETRIES: usize = 1000;

        let mut dummy_retries = 0;
        let mut zip_data = Vec::new();

        loop {
            let flush_id = FLUSH_COUNTER.fetch_add(1, Ordering::Relaxed);
            zip_data.clear();
            let mut cursor = Cursor::new(Vec::new());
            {
                let mut zip = zip::ZipWriter::new(&mut cursor);
                let deflated = FileOptions::<()>::default()
                    .compression_method(zip::CompressionMethod::Deflated);

                // Dummy file – content grows aggressively
                // let dummy_content = "x".repeat(128 * dummy_retries);
                let dummy_content: String = rngs::ThreadRng::default()
                    .sample_iter(&Alphanumeric)
                    .take(1024 * dummy_retries)
                    .map(char::from)
                    .collect();

                zip.start_file("dummy.txt", deflated)?;
                zip.write_all(dummy_content.as_bytes())?;

                let mut manifest = json!({});

                let mut numbers: Vec<usize> = (0..NUM_BUTTONS).collect();
                numbers.shuffle(&mut rngs::ThreadRng::default());
                for (index, value) in numbers.into_iter().enumerate() {
                    if value == PHANTOM_KEY {
                        continue;
                    }
                    let col = value % 5;
                    let row = value / 5;
                    let key = format!("{}_{}", col, row);
                    let mut view_param = json!({ "Text": "" });

                    if let Some(img_data) = images_snapshot.get_mut(&value) {
                        // let icon_name = format!("{}.png", img_data.uuid);
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

            zip_data = cursor.into_inner();

            let file_size = zip_data.len();
            let mut valid = true;
            for offset in (92152..file_size).step_by(1024) {
                if let Some(&byte) = zip_data.get(offset) {
                    if INVALID_BYTES.contains(&byte) {
                        debug!(
                            "Invalid byte 0x{:02x} at offset {} (retry {})",
                            byte, offset, dummy_retries
                        );
                        valid = false;
                        break;
                    }
                }
            }

            if valid {
                debug!("ZIP archive passed the byte‑offset check ({} retries)", dummy_retries);
                break;
            }

            dummy_retries += 1;
            if dummy_retries >= MAX_RETRIES {
                return Err(anyhow!(
                    "Failed to build a valid ZIP after {} retries – giving up",
                    MAX_RETRIES
                ));
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        info!("Sent button configuration ({} bytes)", zip_data.len());
        self.send_file(&zip_data).await?;
        info!("Successfully sent button configuration ({} bytes)", zip_data.len());
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

        if data.len() > 1016 {
            for chunk in data[1016..].chunks(1024) {
                let mut packet = [0u8; PACKET_SIZE];
                let len = chunk.len().min(PACKET_SIZE);
                packet[..len].copy_from_slice(&chunk[..len]);
                writer.write_output_report(&packet).await?;
            }
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
        // Rotation: byte 10 == 2, byte 11 == 2 (left) or 3 (right).
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

            // Press carries state 0/1 and no rotation marker.
            let down = report(0x0101, 1, idx, 0, 1);
            match UlanziDevice::parse_input(&down) {
                Some(InputEvent::EncoderPress { position, pressed }) => {
                    assert_eq!(position, expected_pos);
                    assert!(pressed);
                }
                other => panic!("expected encoder press for {idx}, got {:?}", other),
            }

            let up = report(0x0101, 0, idx, 0, 0);
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
    fn test_parse_input_ignores_unknown_command() {
        let buf = report(0x0999, 1, 3, 0, 1);
        assert!(UlanziDevice::parse_input(&buf).is_none());
    }
}
