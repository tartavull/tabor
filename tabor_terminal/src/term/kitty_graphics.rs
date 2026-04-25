use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as Base64;
use flate2::read::ZlibDecoder;
use image::ImageFormat;
use log::debug;

use crate::index::{Line, Point};

const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_PENDING_BASE64_BYTES: usize = 96 * 1024 * 1024;
const MAX_PLACEMENTS: usize = 4096;
const MAX_GENERATED_IMAGE_ID: u32 = u32::MAX - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KittyCellSize {
    pub width_px: u16,
    pub height_px: u16,
}

impl Default for KittyCellSize {
    fn default() -> Self {
        Self { width_px: 1, height_px: 1 }
    }
}

#[derive(Debug, Clone)]
pub struct KittyGraphicsImage {
    pub storage_id: u64,
    pub protocol_id: u32,
    pub number: u32,
    pub generation: u64,
    pub width_px: u32,
    pub height_px: u32,
    pub rgba: Arc<[u8]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyGraphicsPlacement {
    pub image_storage_id: u64,
    pub image_generation: u64,
    pub protocol_id: u32,
    pub placement_id: u32,
    pub point: Point,
    pub source_x_px: u32,
    pub source_y_px: u32,
    pub source_width_px: u32,
    pub source_height_px: u32,
    pub columns: Option<u32>,
    pub rows: Option<u32>,
    pub cell_columns: u32,
    pub cell_rows: u32,
    pub offset_x_px: u32,
    pub offset_y_px: u32,
    pub z_index: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KittyCursorMove {
    pub columns: usize,
    pub rows: usize,
}

#[derive(Debug, Default)]
pub(crate) struct KittyGraphicsCommandResult {
    pub responses: Vec<String>,
    pub cursor_move: Option<KittyCursorMove>,
    pub changed: bool,
}

#[derive(Debug, Default)]
pub struct KittyGraphicsState {
    images: HashMap<u64, Arc<KittyGraphicsImage>>,
    image_ids: HashMap<u32, u64>,
    image_numbers: HashMap<u32, u64>,
    placements: Vec<KittyGraphicsPlacement>,
    pending_upload: Option<PendingUpload>,
    next_storage_id: u64,
    next_generated_image_id: u32,
    next_generation: u64,
}

#[derive(Debug)]
struct PendingUpload {
    command: KittyGraphicsCommand,
    payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KittyGraphicsCommand {
    action: char,
    quiet: u8,
    format: u32,
    medium: char,
    width_px: u32,
    height_px: u32,
    data_size: Option<usize>,
    data_offset: u64,
    image_id: u32,
    image_number: u32,
    placement_id: u32,
    compression: Option<char>,
    more_chunks: bool,
    source_x_px: u32,
    source_y_px: u32,
    source_width_px: Option<u32>,
    source_height_px: Option<u32>,
    offset_x_px: u32,
    offset_y_px: u32,
    columns: Option<u32>,
    rows: Option<u32>,
    cursor_policy: u32,
    virtual_placement: bool,
    z_index: i32,
    delete: char,
}

impl Default for KittyGraphicsCommand {
    fn default() -> Self {
        Self {
            action: 't',
            quiet: 0,
            format: 32,
            medium: 'd',
            width_px: 0,
            height_px: 0,
            data_size: None,
            data_offset: 0,
            image_id: 0,
            image_number: 0,
            placement_id: 0,
            compression: None,
            more_chunks: false,
            source_x_px: 0,
            source_y_px: 0,
            source_width_px: None,
            source_height_px: None,
            offset_x_px: 0,
            offset_y_px: 0,
            columns: None,
            rows: None,
            cursor_policy: 0,
            virtual_placement: false,
            z_index: 0,
            delete: 'a',
        }
    }
}

#[derive(Debug)]
struct DecodedImage {
    width_px: u32,
    height_px: u32,
    rgba: Arc<[u8]>,
}

impl KittyGraphicsState {
    pub fn placements(&self) -> &[KittyGraphicsPlacement] {
        &self.placements
    }

    pub fn image(&self, storage_id: u64) -> Option<&Arc<KittyGraphicsImage>> {
        self.images.get(&storage_id)
    }

    pub(crate) fn clear(&mut self) {
        self.images.clear();
        self.image_ids.clear();
        self.image_numbers.clear();
        self.placements.clear();
        self.pending_upload = None;
    }

    pub(crate) fn clear_placements(&mut self) {
        self.placements.clear();
    }

    pub(crate) fn resize(&mut self, topmost: Line, lines: usize, columns: usize) {
        let bottom = Line(lines as i32);
        self.placements.retain(|placement| {
            placement.point.column.0 < columns
                && placement.point.line < bottom
                && placement.point.line + placement.cell_rows as i32 > topmost
        });
    }

    pub(crate) fn scroll_up(&mut self, region: std::ops::Range<Line>, lines: usize, topmost: Line) {
        let lines = lines as i32;
        self.placements.retain_mut(|placement| {
            let affected = if region.start == 0 {
                placement.point.line < region.end
            } else {
                placement.point.line >= region.start && placement.point.line < region.end
            };
            if !affected {
                return true;
            }

            placement.point.line -= lines;
            placement.point.line + placement.cell_rows as i32 > topmost
        });
    }

    pub(crate) fn scroll_down(&mut self, region: std::ops::Range<Line>, lines: usize) {
        let lines = lines as i32;
        self.placements.retain_mut(|placement| {
            if placement.point.line < region.start || placement.point.line >= region.end {
                return true;
            }

            placement.point.line += lines;
            placement.point.line < region.end
        });
    }

    pub(crate) fn handle_apc(
        &mut self,
        bytes: &[u8],
        cursor: Point,
        cell_size: KittyCellSize,
    ) -> KittyGraphicsCommandResult {
        let mut result = KittyGraphicsCommandResult::default();
        let (control, payload) = split_apc(bytes);
        let command = match parse_control(control) {
            Ok(command) => command,
            Err(err) => {
                result.responses.push(response(None, None, 0, &format!("EINVAL:{err}")));
                return result;
            },
        };

        if let Some(mut pending) = self.pending_upload.take() {
            if pending.payload.len().saturating_add(payload.len()) > MAX_PENDING_BASE64_BYTES {
                result.responses.push(response(
                    Some(&pending.command),
                    None,
                    pending.command.quiet,
                    "ETOOBIG:chunked image data exceeded limit",
                ));
                return result;
            }

            pending.payload.extend_from_slice(payload);
            if command.more_chunks {
                self.pending_upload = Some(pending);
                return result;
            }

            return self.process_command(pending.command, &pending.payload, cursor, cell_size);
        }

        if command.more_chunks {
            self.pending_upload = Some(PendingUpload { command, payload: payload.to_vec() });
            return result;
        }

        self.process_command(command, payload, cursor, cell_size)
    }

    fn process_command(
        &mut self,
        command: KittyGraphicsCommand,
        payload: &[u8],
        cursor: Point,
        cell_size: KittyCellSize,
    ) -> KittyGraphicsCommandResult {
        let mut result = KittyGraphicsCommandResult::default();
        match command.action {
            'q' => {
                result.responses.push(response_for_result(&command, None, Ok("OK".to_owned())));
            },
            't' | 'T' => match self.load_image_data(&command, payload) {
                Ok(decoded) => match self.store_image(&command, decoded) {
                    Ok(image) => {
                        if command.action == 'T' && !command.virtual_placement {
                            match self.place_image(&command, &image, cursor, cell_size) {
                                Ok(cursor_move) => {
                                    result.cursor_move = cursor_move;
                                    result.changed = true;
                                },
                                Err(err) => result.responses.push(response_for_result(
                                    &command,
                                    Some(image.protocol_id),
                                    Err(err),
                                )),
                            }
                        }
                        result.responses.push(response_for_result(
                            &command,
                            Some(image.protocol_id),
                            Ok("OK".to_owned()),
                        ));
                    },
                    Err(err) => {
                        result.responses.push(response_for_result(&command, None, Err(err)));
                    },
                },
                Err(err) => result.responses.push(response_for_result(&command, None, Err(err))),
            },
            'p' => match self.resolve_image(&command).cloned() {
                Some(image) => match self.place_image(&command, &image, cursor, cell_size) {
                    Ok(cursor_move) => {
                        result.cursor_move = cursor_move;
                        result.changed = true;
                        result.responses.push(response_for_result(
                            &command,
                            Some(image.protocol_id),
                            Ok("OK".to_owned()),
                        ));
                    },
                    Err(err) => result.responses.push(response_for_result(
                        &command,
                        Some(image.protocol_id),
                        Err(err),
                    )),
                },
                None => result.responses.push(response_for_result(
                    &command,
                    None,
                    Err(String::from("ENOENT:image id not found")),
                )),
            },
            'd' => {
                self.delete(&command);
                result.changed = true;
                result.responses.push(response_for_result(&command, None, Ok("OK".to_owned())));
            },
            action => {
                result.responses.push(response_for_result(
                    &command,
                    None,
                    Err(format!("EINVAL:unsupported action {action}")),
                ));
            },
        }

        result
    }

    fn load_image_data(
        &self,
        command: &KittyGraphicsCommand,
        payload: &[u8],
    ) -> Result<DecodedImage, String> {
        let mut data = match command.medium {
            'd' => Base64.decode(payload).map_err(|err| format!("EINVAL:invalid base64: {err}"))?,
            'f' | 't' => read_file_payload(command, payload)?,
            's' => return Err(String::from("EINVAL:shared memory transfer is unsupported")),
            medium => return Err(format!("EINVAL:unsupported transmission medium {medium}")),
        };

        if command.compression == Some('z') {
            let mut inflated = Vec::new();
            ZlibDecoder::new(data.as_slice())
                .take(MAX_IMAGE_BYTES as u64 + 1)
                .read_to_end(&mut inflated)
                .map_err(|err| format!("EINVAL:zlib decode failed: {err}"))?;
            data = inflated;
        } else if command.compression.is_some() {
            return Err(String::from("EINVAL:unsupported compression"));
        }

        if data.len() > MAX_IMAGE_BYTES {
            return Err(String::from("ETOOBIG:image data exceeded limit"));
        }

        match command.format {
            100 => decode_png(&data),
            32 => decode_rgba(command, &data),
            24 => decode_rgb(command, &data),
            format => Err(format!("EINVAL:unsupported pixel format {format}")),
        }
    }

    fn store_image(
        &mut self,
        command: &KittyGraphicsCommand,
        decoded: DecodedImage,
    ) -> Result<Arc<KittyGraphicsImage>, String> {
        let protocol_id = self.protocol_id_for_command(command)?;
        let storage_id =
            if protocol_id == 0 { self.allocate_storage_id() } else { protocol_id as u64 };
        let generation = self.allocate_generation();

        if protocol_id != 0 {
            self.image_ids.insert(protocol_id, storage_id);
            self.placements.retain(|placement| placement.protocol_id != protocol_id);
        }
        if command.image_number != 0 {
            self.image_numbers.insert(command.image_number, storage_id);
        }

        let image = Arc::new(KittyGraphicsImage {
            storage_id,
            protocol_id,
            number: command.image_number,
            generation,
            width_px: decoded.width_px,
            height_px: decoded.height_px,
            rgba: decoded.rgba,
        });
        self.images.insert(storage_id, image.clone());
        Ok(image)
    }

    fn place_image(
        &mut self,
        command: &KittyGraphicsCommand,
        image: &KittyGraphicsImage,
        cursor: Point,
        cell_size: KittyCellSize,
    ) -> Result<Option<KittyCursorMove>, String> {
        let source_x = command.source_x_px.min(image.width_px);
        let source_y = command.source_y_px.min(image.height_px);
        let max_width = image.width_px.saturating_sub(source_x);
        let max_height = image.height_px.saturating_sub(source_y);
        let source_width = command.source_width_px.unwrap_or(max_width).min(max_width);
        let source_height = command.source_height_px.unwrap_or(max_height).min(max_height);
        if source_width == 0 || source_height == 0 {
            return Err(String::from("EINVAL:empty source rectangle"));
        }

        let (cell_columns, cell_rows) =
            placement_cell_size(command, source_width, source_height, cell_size);
        let placement = KittyGraphicsPlacement {
            image_storage_id: image.storage_id,
            image_generation: image.generation,
            protocol_id: image.protocol_id,
            placement_id: if image.protocol_id == 0 { 0 } else { command.placement_id },
            point: cursor,
            source_x_px: source_x,
            source_y_px: source_y,
            source_width_px: source_width,
            source_height_px: source_height,
            columns: command.columns,
            rows: command.rows,
            cell_columns,
            cell_rows,
            offset_x_px: command.offset_x_px,
            offset_y_px: command.offset_y_px,
            z_index: command.z_index,
        };

        if placement.protocol_id != 0 && placement.placement_id != 0 {
            self.placements.retain(|existing| {
                existing.protocol_id != placement.protocol_id
                    || existing.placement_id != placement.placement_id
            });
        }

        self.placements.push(placement);
        if self.placements.len() > MAX_PLACEMENTS {
            self.placements.remove(0);
        }

        Ok((command.cursor_policy != 1).then_some(KittyCursorMove {
            columns: cell_columns as usize,
            rows: cell_rows as usize,
        }))
    }

    fn resolve_image(&self, command: &KittyGraphicsCommand) -> Option<&Arc<KittyGraphicsImage>> {
        if command.image_id != 0 {
            return self.image_ids.get(&command.image_id).and_then(|id| self.images.get(id));
        }
        if command.image_number != 0 {
            return self
                .image_numbers
                .get(&command.image_number)
                .and_then(|id| self.images.get(id));
        }
        None
    }

    fn delete(&mut self, command: &KittyGraphicsCommand) {
        match command.delete {
            'a' => self.placements.clear(),
            'A' => self.clear(),
            'i' | 'I' => self.delete_by_image_id(
                command.image_id,
                command.placement_id,
                command.delete == 'I',
            ),
            'n' | 'N' => {
                if let Some(storage_id) = self.image_numbers.get(&command.image_number).copied() {
                    let protocol_id =
                        self.images.get(&storage_id).map_or(0, |image| image.protocol_id);
                    self.delete_by_image_id(protocol_id, command.placement_id, false);
                    if command.delete == 'N' {
                        self.remove_image(storage_id);
                    }
                }
            },
            'c' | 'C' => self.delete_at_cell(command, None),
            'p' | 'P' => {
                self.delete_at_cell(command, Some((command.source_x_px, command.source_y_px)))
            },
            'x' | 'X' => {
                let column = command.source_x_px;
                self.placements.retain(|placement| {
                    let start = placement.point.column.0 as u32;
                    let end = start.saturating_add(placement.cell_columns);
                    !(column >= start && column < end)
                });
            },
            'y' | 'Y' => {
                let row = command.source_y_px as i32;
                self.placements.retain(|placement| {
                    let start = placement.point.line.0;
                    let end = start.saturating_add(placement.cell_rows as i32);
                    !(row >= start && row < end)
                });
            },
            'z' | 'Z' => self.placements.retain(|placement| placement.z_index != command.z_index),
            other => debug!("unsupported kitty graphics delete selector: {other}"),
        }
    }

    fn delete_at_cell(&mut self, command: &KittyGraphicsCommand, cell: Option<(u32, u32)>) {
        let (column, row) = cell.unwrap_or((command.source_x_px, command.source_y_px));
        let row = row as i32;
        self.placements.retain(|placement| {
            let start_col = placement.point.column.0 as u32;
            let end_col = start_col.saturating_add(placement.cell_columns);
            let start_row = placement.point.line.0;
            let end_row = start_row.saturating_add(placement.cell_rows as i32);
            !(column >= start_col && column < end_col && row >= start_row && row < end_row)
        });
    }

    fn delete_by_image_id(&mut self, protocol_id: u32, placement_id: u32, remove_image: bool) {
        if protocol_id == 0 {
            return;
        }

        self.placements.retain(|placement| {
            placement.protocol_id != protocol_id
                || (placement_id != 0 && placement.placement_id != placement_id)
        });
        if remove_image {
            if let Some(storage_id) = self.image_ids.remove(&protocol_id) {
                self.remove_image(storage_id);
            }
        }
    }

    fn remove_image(&mut self, storage_id: u64) {
        self.images.remove(&storage_id);
        self.image_ids.retain(|_, existing| *existing != storage_id);
        self.image_numbers.retain(|_, existing| *existing != storage_id);
        self.placements.retain(|placement| placement.image_storage_id != storage_id);
    }

    fn protocol_id_for_command(&mut self, command: &KittyGraphicsCommand) -> Result<u32, String> {
        match (command.image_id, command.image_number) {
            (0, 0) => Ok(0),
            (image_id, 0) => Ok(image_id),
            (0, _) => Ok(self.allocate_generated_image_id()),
            _ => Err(String::from("EINVAL:image id and image number are mutually exclusive")),
        }
    }

    fn allocate_storage_id(&mut self) -> u64 {
        self.next_storage_id = self.next_storage_id.saturating_add(1).max(1);
        u64::from(u32::MAX) + self.next_storage_id
    }

    fn allocate_generation(&mut self) -> u64 {
        self.next_generation = self.next_generation.saturating_add(1).max(1);
        self.next_generation
    }

    fn allocate_generated_image_id(&mut self) -> u32 {
        self.next_generated_image_id = self.next_generated_image_id.saturating_add(1).max(1);
        if self.next_generated_image_id >= MAX_GENERATED_IMAGE_ID {
            self.next_generated_image_id = 1;
        }
        while self.image_ids.contains_key(&self.next_generated_image_id) {
            self.next_generated_image_id = self.next_generated_image_id.saturating_add(1).max(1);
            if self.next_generated_image_id >= MAX_GENERATED_IMAGE_ID {
                self.next_generated_image_id = 1;
            }
        }
        self.next_generated_image_id
    }
}

fn split_apc(bytes: &[u8]) -> (&[u8], &[u8]) {
    match bytes.iter().position(|byte| *byte == b';') {
        Some(index) => (&bytes[..index], &bytes[index + 1..]),
        None => (bytes, &[]),
    }
}

fn parse_control(bytes: &[u8]) -> Result<KittyGraphicsCommand, String> {
    let mut command = KittyGraphicsCommand::default();
    if bytes.is_empty() {
        return Ok(command);
    }

    let control =
        std::str::from_utf8(bytes).map_err(|err| format!("control is not utf-8: {err}"))?;
    for pair in control.split(',') {
        if pair.is_empty() {
            continue;
        }
        let Some((key, value)) = pair.split_once('=') else {
            return Err(format!("control item has no value: {pair}"));
        };
        match key {
            "a" => command.action = parse_char(value)?,
            "q" => command.quiet = parse_u32(value)?.min(2) as u8,
            "f" => command.format = parse_u32(value)?,
            "t" => command.medium = parse_char(value)?,
            "s" => command.width_px = parse_u32(value)?,
            "v" => command.height_px = parse_u32(value)?,
            "S" => {
                let size = parse_u64(value)?;
                command.data_size = (size != 0).then_some(
                    usize::try_from(size).map_err(|_| String::from("data size overflows usize"))?,
                );
            },
            "O" => command.data_offset = parse_u64(value)?,
            "i" => command.image_id = parse_u32(value)?,
            "I" => command.image_number = parse_u32(value)?,
            "p" => command.placement_id = parse_u32(value)?,
            "o" => command.compression = Some(parse_char(value)?),
            "m" => command.more_chunks = parse_u32(value)? != 0,
            "x" => command.source_x_px = parse_u32(value)?,
            "y" => command.source_y_px = parse_u32(value)?,
            "w" => command.source_width_px = nonzero_u32(value)?,
            "h" => command.source_height_px = nonzero_u32(value)?,
            "X" => command.offset_x_px = parse_u32(value)?,
            "Y" => command.offset_y_px = parse_u32(value)?,
            "c" => command.columns = nonzero_u32(value)?,
            "r" => command.rows = nonzero_u32(value)?,
            "C" => command.cursor_policy = parse_u32(value)?,
            "U" => command.virtual_placement = parse_u32(value)? == 1,
            "z" => command.z_index = parse_i32(value)?,
            "d" => command.delete = parse_char(value)?,
            _ => (),
        }
    }

    Ok(command)
}

fn nonzero_u32(value: &str) -> Result<Option<u32>, String> {
    let value = parse_u32(value)?;
    Ok((value != 0).then_some(value))
}

fn parse_char(value: &str) -> Result<char, String> {
    let mut chars = value.chars();
    let Some(ch) = chars.next() else {
        return Err(String::from("empty character value"));
    };
    if chars.next().is_some() {
        return Err(format!("expected single character, got {value}"));
    }
    Ok(ch)
}

fn parse_u32(value: &str) -> Result<u32, String> {
    value.parse::<u32>().map_err(|err| format!("invalid integer {value}: {err}"))
}

fn parse_u64(value: &str) -> Result<u64, String> {
    value.parse::<u64>().map_err(|err| format!("invalid integer {value}: {err}"))
}

fn parse_i32(value: &str) -> Result<i32, String> {
    value.parse::<i32>().map_err(|err| format!("invalid integer {value}: {err}"))
}

fn read_file_payload(command: &KittyGraphicsCommand, payload: &[u8]) -> Result<Vec<u8>, String> {
    let path_bytes =
        Base64.decode(payload).map_err(|err| format!("EINVAL:invalid file path base64: {err}"))?;
    let path = std::str::from_utf8(&path_bytes)
        .map_err(|err| format!("EINVAL:file path is not utf-8: {err}"))?;
    let path = Path::new(path);
    reject_sensitive_path(path)?;

    let metadata =
        fs::metadata(path).map_err(|err| format!("ENOENT:file metadata failed: {err}"))?;
    if !metadata.is_file() {
        return Err(String::from("EINVAL:graphics path is not a regular file"));
    }

    let size = command
        .data_size
        .unwrap_or_else(|| metadata.len().saturating_sub(command.data_offset) as usize);
    if size > MAX_IMAGE_BYTES {
        return Err(String::from("ETOOBIG:file image data exceeded limit"));
    }

    let mut file = File::open(path).map_err(|err| format!("EIO:file open failed: {err}"))?;
    file.seek(SeekFrom::Start(command.data_offset))
        .map_err(|err| format!("EIO:file seek failed: {err}"))?;
    let mut data = Vec::with_capacity(size);
    file.take(size as u64)
        .read_to_end(&mut data)
        .map_err(|err| format!("EIO:file read failed: {err}"))?;

    if command.medium == 't' && safe_to_delete_temporary_file(path) {
        let _ = fs::remove_file(path);
    }

    Ok(data)
}

fn reject_sensitive_path(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        for prefix in ["/dev", "/proc", "/sys"] {
            if path.starts_with(prefix) {
                return Err(String::from("EPERM:refusing sensitive graphics path"));
            }
        }
    }
    Ok(())
}

fn safe_to_delete_temporary_file(path: &Path) -> bool {
    let path_text = path.to_string_lossy();
    if !path_text.contains("tty-graphics-protocol") {
        return false;
    }

    let temp_dir = std::env::temp_dir();
    path.starts_with(&temp_dir) || path.starts_with("/tmp") || path.starts_with("/dev/shm")
}

fn decode_png(data: &[u8]) -> Result<DecodedImage, String> {
    let image = image::load_from_memory_with_format(data, ImageFormat::Png)
        .map_err(|err| format!("EINVAL:png decode failed: {err}"))?
        .to_rgba8();
    let (width_px, height_px) = image.dimensions();
    Ok(DecodedImage { width_px, height_px, rgba: Arc::from(image.into_raw()) })
}

fn decode_rgba(command: &KittyGraphicsCommand, data: &[u8]) -> Result<DecodedImage, String> {
    let expected = expected_raw_len(command, 4)?;
    if data.len() < expected {
        return Err(String::from("EINVAL:rgba payload is shorter than dimensions"));
    }
    Ok(DecodedImage {
        width_px: command.width_px,
        height_px: command.height_px,
        rgba: Arc::from(&data[..expected]),
    })
}

fn decode_rgb(command: &KittyGraphicsCommand, data: &[u8]) -> Result<DecodedImage, String> {
    let expected = expected_raw_len(command, 3)?;
    if data.len() < expected {
        return Err(String::from("EINVAL:rgb payload is shorter than dimensions"));
    }

    let mut rgba = Vec::with_capacity(command.width_px as usize * command.height_px as usize * 4);
    for pixel in data[..expected].chunks_exact(3) {
        rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
    }
    Ok(DecodedImage {
        width_px: command.width_px,
        height_px: command.height_px,
        rgba: Arc::from(rgba),
    })
}

fn expected_raw_len(command: &KittyGraphicsCommand, channels: usize) -> Result<usize, String> {
    if command.width_px == 0 || command.height_px == 0 {
        return Err(String::from("EINVAL:raw pixel data requires width and height"));
    }

    command
        .width_px
        .checked_mul(command.height_px)
        .and_then(|pixels| pixels.checked_mul(channels as u32))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .filter(|bytes| *bytes <= MAX_IMAGE_BYTES)
        .ok_or_else(|| String::from("ETOOBIG:raw image dimensions exceeded limit"))
}

fn placement_cell_size(
    command: &KittyGraphicsCommand,
    source_width: u32,
    source_height: u32,
    cell_size: KittyCellSize,
) -> (u32, u32) {
    let cell_width = u32::from(cell_size.width_px.max(1));
    let cell_height = u32::from(cell_size.height_px.max(1));
    match (command.columns, command.rows) {
        (Some(columns), Some(rows)) => (columns.max(1), rows.max(1)),
        (Some(columns), None) => {
            let width_px = columns.saturating_mul(cell_width);
            let height_px = scale_extent(width_px, source_height, source_width);
            (columns.max(1), div_ceil(height_px, cell_height).max(1))
        },
        (None, Some(rows)) => {
            let height_px = rows.saturating_mul(cell_height);
            let width_px = scale_extent(height_px, source_width, source_height);
            (div_ceil(width_px, cell_width).max(1), rows.max(1))
        },
        (None, None) => {
            (div_ceil(source_width, cell_width).max(1), div_ceil(source_height, cell_height).max(1))
        },
    }
}

fn scale_extent(known_extent: u32, numerator: u32, denominator: u32) -> u32 {
    if denominator == 0 {
        return 1;
    }
    let extent = u64::from(known_extent).saturating_mul(u64::from(numerator));
    div_ceil_u64(extent, u64::from(denominator)).min(u64::from(u32::MAX)) as u32
}

fn div_ceil(numerator: u32, denominator: u32) -> u32 {
    div_ceil_u64(u64::from(numerator), u64::from(denominator)).min(u64::from(u32::MAX)) as u32
}

fn div_ceil_u64(numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 { 1 } else { numerator.div_ceil(denominator) }
}

fn response_for_result(
    command: &KittyGraphicsCommand,
    image_id: Option<u32>,
    result: Result<String, String>,
) -> String {
    let status = match result {
        Ok(status) => status,
        Err(err) => err,
    };
    response(Some(command), image_id, command.quiet, &status)
}

fn response(
    command: Option<&KittyGraphicsCommand>,
    image_id: Option<u32>,
    quiet: u8,
    status: &str,
) -> String {
    let is_ok = status == "OK";
    if (is_ok && quiet >= 1) || (!is_ok && quiet >= 2) {
        return String::new();
    }

    let mut control = String::new();
    let image_id = image_id.or_else(|| command.map(|command| command.image_id)).unwrap_or(0);
    if image_id != 0 {
        control.push_str(&format!("i={image_id}"));
    }
    if let Some(command) = command {
        if command.image_number != 0 {
            if !control.is_empty() {
                control.push(',');
            }
            control.push_str(&format!("I={}", command.image_number));
        }
        if command.placement_id != 0 {
            if !control.is_empty() {
                control.push(',');
            }
            control.push_str(&format!("p={}", command.placement_id));
        }
    }

    format!("\x1b_G{control};{status}\x1b\\")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Column;

    fn command_result(state: &mut KittyGraphicsState, data: &[u8]) -> KittyGraphicsCommandResult {
        state.handle_apc(
            data,
            Point::new(Line(0), Column(0)),
            KittyCellSize { width_px: 8, height_px: 16 },
        )
    }

    #[test]
    fn direct_rgba_transmit_and_display_creates_placement() {
        let mut state = KittyGraphicsState::default();
        let result = command_result(&mut state, b"a=T,f=32,s=1,v=1,c=2,r=1;/////w==");

        assert!(result.changed);
        assert_eq!(result.cursor_move, Some(KittyCursorMove { columns: 2, rows: 1 }));
        assert_eq!(state.images.len(), 1);
        assert_eq!(state.placements.len(), 1);
        assert_eq!(state.placements[0].cell_columns, 2);
        assert_eq!(state.placements[0].cell_rows, 1);
    }

    #[test]
    fn chunked_direct_payload_is_displayed_after_final_chunk() {
        let mut state = KittyGraphicsState::default();
        let first = command_result(&mut state, b"a=T,f=32,s=1,v=1,c=1,r=1,m=1;////");
        assert!(!first.changed);
        assert_eq!(state.placements.len(), 0);

        let second = command_result(&mut state, b"m=0;/w==");
        assert!(second.changed);
        assert_eq!(state.placements.len(), 1);
    }

    #[test]
    fn chunked_direct_payload_accepts_empty_final_chunk() {
        let mut state = KittyGraphicsState::default();
        let first = command_result(&mut state, b"a=T,f=32,s=1,v=1,c=1,r=1,m=1;/////w==");
        assert!(!first.changed);
        assert_eq!(state.placements.len(), 0);

        let second = command_result(&mut state, b"m=0;");
        assert!(second.changed);
        assert_eq!(state.placements.len(), 1);
    }

    #[test]
    fn put_reuses_transmitted_image_id() {
        let mut state = KittyGraphicsState::default();
        let transmit = command_result(&mut state, b"a=t,i=9,f=32,s=1,v=1;/////w==");
        assert!(!transmit.changed);
        assert_eq!(state.images.len(), 1);

        let put = command_result(&mut state, b"a=p,i=9,c=3,r=2");
        assert!(put.changed);
        assert_eq!(state.placements.len(), 1);
        assert_eq!(state.placements[0].protocol_id, 9);
        assert_eq!(state.placements[0].cell_columns, 3);
        assert_eq!(state.placements[0].cell_rows, 2);
    }

    #[test]
    fn top_scroll_keeps_placements_moving_in_scrollback() {
        let mut state = KittyGraphicsState::default();
        let result = command_result(&mut state, b"a=T,f=32,s=1,v=1,c=2,r=3;/////w==");
        assert!(result.changed);

        state.scroll_up(Line(0)..Line(24), 1, Line(-100));
        state.scroll_up(Line(0)..Line(24), 1, Line(-100));

        assert_eq!(state.placements.len(), 1);
        assert_eq!(state.placements[0].point.line, Line(-2));

        state.scroll_up(Line(0)..Line(24), 5, Line(-4));
        assert!(state.placements.is_empty());
    }

    #[test]
    fn resize_retains_placements_inside_scrollback() {
        let mut state = KittyGraphicsState::default();
        let result = command_result(&mut state, b"a=T,f=32,s=1,v=1,c=2,r=3;/////w==");
        assert!(result.changed);

        state.scroll_up(Line(0)..Line(24), 5, Line(-100));
        assert_eq!(state.placements[0].point.line, Line(-5));

        state.resize(Line(-6), 24, 80);
        assert_eq!(state.placements.len(), 1);

        state.resize(Line(-2), 24, 80);
        assert!(state.placements.is_empty());
    }
}
