//! Browser-free input state for the native Android host.
//!
//! The engine input block is a caller-owned 444-byte region.  This crate only
//! writes the input and platform fields documented in [`offsets`].  In
//! particular, the debug draw limit and per-frame draw count remain untouched,
//! and the text tail is left for the engine consumer to advance.

use std::collections::HashMap;
use std::fmt;

pub const SHARED_BLOCK_SIZE: usize = 444;
pub const KEY_COUNT: usize = 256;
pub const TEXT_CAPACITY: usize = 64;
pub const KEY_DOWN_VALUE: u8 = 0x80;

/// Byte offsets in the shared Wasm input block.
pub mod offsets {
    pub const KEYS: usize = 0;
    pub const MOUSE_X: usize = 256;
    pub const MOUSE_Y: usize = 260;
    pub const MOUSE_DX: usize = 264;
    pub const MOUSE_DY: usize = 268;
    pub const MOUSE_DWHEEL: usize = 272;
    pub const MOUSE_BUTTONS: usize = 276;
    pub const FOCUS: usize = 280;
    pub const LOCKED: usize = 284;
    pub const TEXT_HEAD: usize = 288;
    /// Engine-owned.  The host reads this value to apply backpressure.
    pub const TEXT_TAIL: usize = 292;
    pub const TEXT: usize = 296;
    /// Engine-owned debug draw limit and draw count.
    pub const DEBUG_DRAW_LIMIT: usize = 424;
    pub const DEBUG_FRAME_DRAWS: usize = 428;
    pub const WINDOW_WIDTH: usize = 432;
    pub const WINDOW_HEIGHT: usize = 436;
    pub const START_MODE: usize = 440;
}

pub const VK_SHIFT_LEFT: VirtualKey = VirtualKey::new(0xA0);
pub const VK_P: VirtualKey = VirtualKey::new(0x50);
pub const VK_BACKSPACE: VirtualKey = VirtualKey::new(0x08);
pub const VK_TAB: VirtualKey = VirtualKey::new(0x09);
pub const VK_CONTROL_LEFT: VirtualKey = VirtualKey::new(0xA2);
pub const VK_ALT_LEFT: VirtualKey = VirtualKey::new(0xA4);
pub const VK_CAPS_LOCK: VirtualKey = VirtualKey::new(0x14);
pub const VK_DELETE: VirtualKey = VirtualKey::new(0x2E);
pub const VK_H: VirtualKey = VirtualKey::new(0x48);
pub const VK_C: VirtualKey = VirtualKey::new(0x43);
pub const VK_V: VirtualKey = VirtualKey::new(0x56);
pub const VK_W: VirtualKey = VirtualKey::new(0x57);
pub const VK_A: VirtualKey = VirtualKey::new(0x41);
pub const VK_S: VirtualKey = VirtualKey::new(0x53);
pub const VK_D: VirtualKey = VirtualKey::new(0x44);
pub const VK_SPACE: VirtualKey = VirtualKey::new(0x20);
pub const VK_ENTER: VirtualKey = VirtualKey::new(0x0D);
pub const VK_ESCAPE: VirtualKey = VirtualKey::new(0x1B);
pub const VK_Q: VirtualKey = VirtualKey::new(0x51);
pub const VK_E: VirtualKey = VirtualKey::new(0x45);
pub const VK_F: VirtualKey = VirtualKey::new(0x46);
pub const VK_R: VirtualKey = VirtualKey::new(0x52);
pub const VK_Y: VirtualKey = VirtualKey::new(0x59);
pub const VK_SHIFT_RIGHT: VirtualKey = VirtualKey::new(0xA1);
pub const VK_ARROW_LEFT: VirtualKey = VirtualKey::new(0x25);
pub const VK_ARROW_UP: VirtualKey = VirtualKey::new(0x26);
pub const VK_ARROW_RIGHT: VirtualKey = VirtualKey::new(0x27);
pub const VK_ARROW_DOWN: VirtualKey = VirtualKey::new(0x28);
pub const VK_NUMPAD_2: VirtualKey = VirtualKey::new(0x62);
pub const VK_NUMPAD_4: VirtualKey = VirtualKey::new(0x64);
pub const VK_NUMPAD_6: VirtualKey = VirtualKey::new(0x66);
pub const VK_NUMPAD_7: VirtualKey = VirtualKey::new(0x67);
pub const VK_NUMPAD_8: VirtualKey = VirtualKey::new(0x68);
pub const VK_NUMPAD_9: VirtualKey = VirtualKey::new(0x69);

/// A raw Windows virtual-key byte.  All values from 0 through 255 are valid.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VirtualKey(u8);

impl VirtualKey {
    pub const fn new(code: u8) -> Self {
        Self(code)
    }

    pub const fn code(self) -> u8 {
        self.0
    }

    pub fn from_u16(code: u16) -> Option<Self> {
        u8::try_from(code).ok().map(Self)
    }

    pub fn all() -> impl Iterator<Item = Self> {
        (0..=u8::MAX).map(Self)
    }
}

/// Identifies one physical or logical input source.
///
/// A source can hold several keys.  A key can also be held by several sources
/// at once, which prevents one finger or macro release from creating a stuck
/// or prematurely released key.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceId(u32);

impl SourceId {
    pub const KEYBOARD: Self = Self(1);
    pub const MOUSE: Self = Self(2);
    pub const CONTROLLER: Self = Self(3);

    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    pub const fn touch(pointer_id: u32) -> Self {
        Self(0x1000_0000 | pointer_id)
    }

    pub const fn macro_key(macro_id: u32) -> Self {
        Self(0x2000_0000 | macro_id)
    }

    pub const fn id(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum StartMode {
    Story = 1,
    GtaSandbox = 2,
    EnvironmentTest = 3,
}

impl Default for StartMode {
    fn default() -> Self {
        Self::GtaSandbox
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseButton {
    Left = 0,
    Middle = 1,
    Right = 2,
    X1 = 3,
    X2 = 4,
}

impl MouseButton {
    pub const COUNT: usize = 5;

    pub const fn bit(self) -> u32 {
        match self {
            Self::Left => 1 << 0,
            // The engine follows Windows button bits.  DOM button 1 is
            // middle and DOM button 2 is right, so the source maps them to
            // bits 2 and 1 respectively.
            Self::Right => 1 << 1,
            Self::Middle => 1 << 2,
            Self::X1 => 1 << 3,
            Self::X2 => 1 << 4,
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::Left),
            1 => Some(Self::Middle),
            2 => Some(Self::Right),
            3 => Some(Self::X1),
            4 => Some(Self::X2),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MouseDelta {
    pub dx: i32,
    pub dy: i32,
    pub wheel: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MousePosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputError {
    SharedBlockTooSmall { actual: usize, required: usize },
    RefCountOverflow,
    TextRingFull { needed: usize, available: usize },
    InvalidTextTail { head: u32, tail: u32 },
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SharedBlockTooSmall { actual, required } => {
                write!(f, "shared input block is {actual} bytes; need {required}")
            }
            Self::RefCountOverflow => f.write_str("input reference count overflow"),
            Self::TextRingFull { needed, available } => {
                write!(f, "text ring needs {needed} slots; only {available} available")
            }
            Self::InvalidTextTail { head, tail } => {
                write!(f, "engine text tail {tail:#x} is ahead of head {head:#x}")
            }
        }
    }
}

impl std::error::Error for InputError {}

#[derive(Clone, Debug)]
struct KeyLedger {
    totals: [u16; KEY_COUNT],
    source_refs: HashMap<(SourceId, u8), u16>,
}

impl Default for KeyLedger {
    fn default() -> Self {
        Self {
            totals: [0; KEY_COUNT],
            source_refs: HashMap::new(),
        }
    }
}

impl KeyLedger {
    fn press(&mut self, source: SourceId, key: VirtualKey) -> Result<(), InputError> {
        let index = key.code() as usize;
        let source_key = (source, key.code());
        let source_count = self.source_refs.get(&source_key).copied().unwrap_or(0);
        if source_count == u16::MAX || self.totals[index] == u16::MAX {
            return Err(InputError::RefCountOverflow);
        }
        self.source_refs.insert(source_key, source_count + 1);
        self.totals[index] += 1;
        Ok(())
    }

    fn set_down(
        &mut self,
        source: SourceId,
        key: VirtualKey,
        down: bool,
    ) -> Result<(), InputError> {
        if down {
            if self.source_refs.contains_key(&(source, key.code())) {
                Ok(())
            } else {
                self.press(source, key)
            }
        } else {
            self.release_all_for_key(source, key);
            Ok(())
        }
    }

    fn release(&mut self, source: SourceId, key: VirtualKey) -> bool {
        let source_key = (source, key.code());
        let Some(count) = self.source_refs.get_mut(&source_key) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            self.source_refs.remove(&source_key);
        }
        self.totals[key.code() as usize] -= 1;
        true
    }

    fn release_all_for_key(&mut self, source: SourceId, key: VirtualKey) -> bool {
        let Some(count) = self.source_refs.remove(&(source, key.code())) else {
            return false;
        };
        self.totals[key.code() as usize] -= count;
        true
    }

    fn release_source(&mut self, source: SourceId) {
        let held: Vec<(u8, u16)> = self
            .source_refs
            .iter()
            .filter_map(|((held_source, key), count)| {
                (*held_source == source).then_some((*key, *count))
            })
            .collect();
        for (key, count) in held {
            self.source_refs.remove(&(source, key));
            self.totals[key as usize] -= count;
        }
    }

    fn clear(&mut self) {
        self.source_refs.clear();
        self.totals = [0; KEY_COUNT];
    }

    fn is_down(&self, key: VirtualKey) -> bool {
        self.totals[key.code() as usize] != 0
    }

    fn held_count(&self, key: VirtualKey) -> u16 {
        self.totals[key.code() as usize]
    }

    fn source_ref_count(&self, source: SourceId, key: VirtualKey) -> u16 {
        self.source_refs
            .get(&(source, key.code()))
            .copied()
            .unwrap_or(0)
    }

    fn active_count(&self) -> usize {
        self.totals.iter().filter(|count| **count != 0).count()
    }

    fn encode(&self, block: &mut [u8]) {
        for (dst, count) in block[..KEY_COUNT].iter_mut().zip(self.totals) {
            *dst = if count == 0 { 0 } else { KEY_DOWN_VALUE };
        }
    }
}

#[derive(Default, Clone, Debug)]
struct ButtonLedger {
    totals: [u16; MouseButton::COUNT],
    source_refs: HashMap<(SourceId, u8), u16>,
}

impl ButtonLedger {
    fn press(&mut self, source: SourceId, button: MouseButton) -> Result<(), InputError> {
        let index = button.index();
        let source_key = (source, index as u8);
        let source_count = self.source_refs.get(&source_key).copied().unwrap_or(0);
        if source_count == u16::MAX || self.totals[index] == u16::MAX {
            return Err(InputError::RefCountOverflow);
        }
        self.source_refs.insert(source_key, source_count + 1);
        self.totals[index] += 1;
        Ok(())
    }

    fn release(&mut self, source: SourceId, button: MouseButton) -> bool {
        let source_key = (source, button.index() as u8);
        let Some(count) = self.source_refs.get_mut(&source_key) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            self.source_refs.remove(&source_key);
        }
        self.totals[button.index()] -= 1;
        true
    }

    fn release_source(&mut self, source: SourceId) {
        let held: Vec<(u8, u16)> = self
            .source_refs
            .iter()
            .filter_map(|((held_source, button), count)| {
                (*held_source == source).then_some((*button, *count))
            })
            .collect();
        for (button, count) in held {
            self.source_refs.remove(&(source, button));
            self.totals[button as usize] -= count;
        }
    }

    fn clear(&mut self) {
        self.source_refs.clear();
        self.totals = [0; MouseButton::COUNT];
    }

    fn mask(&self) -> u32 {
        self.totals
            .iter()
            .enumerate()
            .filter_map(|(index, count)| {
                (*count != 0)
                    .then_some(MouseButton::from_index(index).map(MouseButton::bit))
                    .flatten()
            })
            .fold(0, |mask, bit| mask | bit)
    }

    fn is_down(&self, button: MouseButton) -> bool {
        self.totals[button.index()] != 0
    }
}

#[derive(Clone, Debug)]
struct MouseState {
    position: MousePosition,
    delta: MouseDelta,
    buttons: ButtonLedger,
    width: i32,
    height: i32,
}

impl MouseState {
    fn new(width: i32, height: i32) -> Self {
        Self {
            position: MousePosition {
                x: width / 2,
                y: height / 2,
            },
            delta: MouseDelta {
                dx: 0,
                dy: 0,
                wheel: 0,
            },
            buttons: ButtonLedger::default(),
            width,
            height,
        }
    }

    fn clamp_position(&self, x: i32, y: i32) -> MousePosition {
        MousePosition {
            x: x.clamp(0, self.width.saturating_sub(1)),
            y: y.clamp(0, self.height.saturating_sub(1)),
        }
    }

    fn set_resolution(&mut self, width: i32, height: i32) {
        self.width = width;
        self.height = height;
        self.position = self.clamp_position(self.position.x, self.position.y);
    }

    fn move_absolute(&mut self, x: i32, y: i32) {
        let next = self.clamp_position(x, y);
        self.delta.dx = self.delta.dx.saturating_add(next.x - self.position.x);
        self.delta.dy = self.delta.dy.saturating_add(next.y - self.position.y);
        self.position = next;
    }

    fn move_relative(&mut self, dx: i32, dy: i32) {
        let next = self.clamp_position(
            self.position.x.saturating_add(dx),
            self.position.y.saturating_add(dy),
        );
        self.delta.dx = self.delta.dx.saturating_add(next.x - self.position.x);
        self.delta.dy = self.delta.dy.saturating_add(next.y - self.position.y);
        self.position = next;
    }

    fn add_wheel(&mut self, wheel: i32) {
        self.delta.wheel = self.delta.wheel.saturating_add(wheel);
    }

    fn drain_delta(&mut self) -> MouseDelta {
        let delta = self.delta;
        self.delta = MouseDelta {
            dx: 0,
            dy: 0,
            wheel: 0,
        };
        delta
    }
}

#[derive(Clone, Debug)]
struct TextRing {
    head: u32,
    /// The engine owns the tail.  This is the last value observed from it.
    tail: u32,
    cells: [u16; TEXT_CAPACITY],
}

impl Default for TextRing {
    fn default() -> Self {
        Self {
            head: 0,
            tail: 0,
            cells: [0; TEXT_CAPACITY],
        }
    }
}

impl TextRing {
    fn queued(&self) -> usize {
        self.head.wrapping_sub(self.tail) as usize
    }

    fn available(&self) -> usize {
        TEXT_CAPACITY.saturating_sub(self.queued())
    }

    fn set_tail(&mut self, tail: u32) -> Result<(), InputError> {
        let distance = self.head.wrapping_sub(tail) as usize;
        if distance > TEXT_CAPACITY {
            return Err(InputError::InvalidTextTail {
                head: self.head,
                tail,
            });
        }
        self.tail = tail;
        Ok(())
    }

    fn push_units(&mut self, units: &[u16]) -> Result<(), InputError> {
        if self.available() < units.len() {
            return Err(InputError::TextRingFull {
                needed: units.len(),
                available: self.available(),
            });
        }
        for unit in units {
            self.cells[(self.head as usize) & (TEXT_CAPACITY - 1)] = *unit;
            self.head = self.head.wrapping_add(1);
        }
        Ok(())
    }

    fn drain(&mut self, max_units: usize) -> Vec<u16> {
        let count = max_units.min(self.queued());
        let mut result = Vec::with_capacity(count);
        for _ in 0..count {
            result.push(self.cells[(self.tail as usize) & (TEXT_CAPACITY - 1)]);
            self.tail = self.tail.wrapping_add(1);
        }
        result
    }
}

/// Mutable native state which can be published into the engine's shared block.
#[derive(Clone, Debug)]
pub struct InputState {
    keys: KeyLedger,
    mouse: MouseState,
    text: TextRing,
    focused: bool,
    locked: bool,
    width: i32,
    height: i32,
    start_mode: StartMode,
}

impl Default for InputState {
    fn default() -> Self {
        Self::new(1, 1)
    }
}

impl InputState {
    pub fn new(width: u32, height: u32) -> Self {
        let (width, height) = normalized_resolution(width, height);
        Self {
            keys: KeyLedger::default(),
            mouse: MouseState::new(width, height),
            text: TextRing::default(),
            focused: false,
            locked: false,
            width,
            height,
            start_mode: StartMode::default(),
        }
    }

    pub fn focus(&self) -> bool {
        self.focused
    }

    pub fn set_focus(&mut self, focused: bool) {
        if !focused {
            self.release_all_inputs();
        }
        self.focused = focused;
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    pub fn resolution(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }

    pub fn set_resolution(&mut self, width: u32, height: u32) {
        let (width, height) = normalized_resolution(width, height);
        self.width = width;
        self.height = height;
        self.mouse.set_resolution(width, height);
    }

    pub fn start_mode(&self) -> StartMode {
        self.start_mode
    }

    pub fn set_start_mode(&mut self, mode: StartMode) {
        self.start_mode = mode;
    }

    pub fn key_press(&mut self, source: SourceId, key: VirtualKey) -> Result<(), InputError> {
        self.keys.press(source, key)
    }

    /// Idempotent key state update for Android key-repeat callbacks.
    pub fn key_set_down(
        &mut self,
        source: SourceId,
        key: VirtualKey,
        down: bool,
    ) -> Result<(), InputError> {
        self.keys.set_down(source, key, down)
    }

    pub fn key_release(&mut self, source: SourceId, key: VirtualKey) -> bool {
        self.keys.release(source, key)
    }

    pub fn key_cancel(&mut self, source: SourceId, key: VirtualKey) -> bool {
        self.keys.release_all_for_key(source, key)
    }

    pub fn cancel_source(&mut self, source: SourceId) {
        self.keys.release_source(source);
        self.mouse.buttons.release_source(source);
    }

    pub fn release_all_inputs(&mut self) {
        self.keys.clear();
        self.mouse.buttons.clear();
        self.mouse.drain_delta();
        self.locked = false;
    }

    pub fn key_down(&self, key: VirtualKey) -> bool {
        self.keys.is_down(key)
    }

    pub fn key_ref_count(&self, key: VirtualKey) -> u16 {
        self.keys.held_count(key)
    }

    pub fn source_key_ref_count(&self, source: SourceId, key: VirtualKey) -> u16 {
        self.keys.source_ref_count(source, key)
    }

    pub fn active_key_count(&self) -> usize {
        self.keys.active_count()
    }

    pub fn mouse_button_press(
        &mut self,
        source: SourceId,
        button: MouseButton,
    ) -> Result<(), InputError> {
        self.mouse.buttons.press(source, button)
    }

    pub fn mouse_button_release(&mut self, source: SourceId, button: MouseButton) -> bool {
        self.mouse.buttons.release(source, button)
    }

    pub fn mouse_button_down(&self, button: MouseButton) -> bool {
        self.mouse.buttons.is_down(button)
    }

    pub fn mouse_position(&self) -> MousePosition {
        self.mouse.position
    }

    pub fn mouse_move_absolute(&mut self, x: i32, y: i32) {
        self.mouse.move_absolute(x, y);
    }

    pub fn mouse_move_relative(&mut self, dx: i32, dy: i32) {
        self.mouse.move_relative(dx, dy);
    }

    pub fn mouse_wheel(&mut self, wheel: i32) {
        self.mouse.add_wheel(wheel);
    }

    pub fn mouse_deltas(&self) -> MouseDelta {
        self.mouse.delta
    }

    pub fn drain_mouse_deltas(&mut self) -> MouseDelta {
        self.mouse.drain_delta()
    }

    pub fn push_text_code_units(&mut self, units: &[u16]) -> Result<(), InputError> {
        self.text.push_units(units)
    }

    pub fn push_text(&mut self, text: &str) -> Result<(), InputError> {
        let units: Vec<u16> = text.encode_utf16().collect();
        self.push_text_code_units(&units)
    }

    pub fn text_queued(&self) -> usize {
        self.text.queued()
    }

    pub fn text_available(&self) -> usize {
        self.text.available()
    }

    pub fn text_head(&self) -> u32 {
        self.text.head
    }

    pub fn text_tail(&self) -> u32 {
        self.text.tail
    }

    /// Test/helper API which simulates the engine consuming text.
    pub fn drain_text(&mut self, max_units: usize) -> Vec<u16> {
        self.text.drain(max_units)
    }

    /// Read the engine-owned tail from a shared block before enqueueing text.
    pub fn sync_text_tail_from_block(&mut self, block: &[u8]) -> Result<(), InputError> {
        ensure_block(block.len())?;
        let tail = read_u32(block, offsets::TEXT_TAIL);
        self.text.set_tail(tail)
    }

    /// Publish host-owned fields into a caller-owned block.
    ///
    /// This method is bounds checked and never writes outside the first 444
    /// bytes.  It deliberately leaves `TEXT_TAIL`, debug draw fields, and any
    /// bytes after the block untouched.  The host should synchronize with its
    /// engine handoff when using a concurrently shared allocation.
    pub fn publish_into(&self, block: &mut [u8]) -> Result<(), InputError> {
        ensure_block(block.len())?;
        self.keys.encode(block);

        write_i32(block, offsets::MOUSE_X, self.mouse.position.x);
        write_i32(block, offsets::MOUSE_Y, self.mouse.position.y);
        write_i32(block, offsets::MOUSE_DX, self.mouse.delta.dx);
        write_i32(block, offsets::MOUSE_DY, self.mouse.delta.dy);
        write_i32(block, offsets::MOUSE_DWHEEL, self.mouse.delta.wheel);
        write_i32(block, offsets::MOUSE_BUTTONS, self.mouse.buttons.mask() as i32);
        write_i32(block, offsets::FOCUS, self.focused as i32);
        write_i32(block, offsets::LOCKED, self.locked as i32);
        write_u32(block, offsets::TEXT_HEAD, self.text.head);

        for (index, unit) in self.text.cells.iter().enumerate() {
            let offset = offsets::TEXT + index * 2;
            block[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }

        write_i32(block, offsets::WINDOW_WIDTH, self.width);
        write_i32(block, offsets::WINDOW_HEIGHT, self.height);
        write_i32(block, offsets::START_MODE, self.start_mode as i32);
        Ok(())
    }
}

fn normalized_resolution(width: u32, height: u32) -> (i32, i32) {
    (
        width.max(1).min(i32::MAX as u32) as i32,
        height.max(1).min(i32::MAX as u32) as i32,
    )
}

fn ensure_block(actual: usize) -> Result<(), InputError> {
    if actual < SHARED_BLOCK_SIZE {
        Err(InputError::SharedBlockTooSmall {
            actual,
            required: SHARED_BLOCK_SIZE,
        })
    } else {
        Ok(())
    }
}

fn write_i32(block: &mut [u8], offset: usize, value: i32) {
    block[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(block: &mut [u8], offset: usize, value: u32) {
    block[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn read_u32(block: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(block[offset..offset + 4].try_into().unwrap())
}

/// A logical control target.  Analog sticks call the input state's absolute
/// or relative mouse methods directly; the rest use these digital bindings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Binding {
    Key(VirtualKey),
    Chord(VirtualKey, VirtualKey),
    MouseButton(MouseButton),
    MouseAxis,
    MouseWheel(i32),
    ContextMenu,
    RadialMenu,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TouchAction {
    MoveForward,
    MoveBackward,
    MoveLeft,
    MoveRight,
    Camera,
    Sprint,
    Jump,
    Melee,
    EnterExit,
    Fire,
    Aim,
    Cover,
    Weapons,
    Gas,
    Brake,
    Handbrake,
    Shoot,
    Horn,
    Radio,
    Lights,
    LookBack,
    CameraView,
    SwitchCharacter,
    Special,
    Reload,
    NextWeapon,
    Pause,
    Map,
    MenuSelect,
    MenuBack,
    MenuTabPrevious,
    MenuTabNext,
    MenuApply,
    MenuDelete,
    DebugKeyboardMode,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
    FaceSouth,
    FaceEast,
    FaceWest,
    FaceNorth,
    LeftBumper,
    RightBumper,
    LeftTrigger,
    RightTrigger,
    LeftStickPress,
    RightStickPress,
    Start,
    Back,
    Context,
    Radial,
    AircraftPitchUp,
    AircraftPitchDown,
    AircraftRollLeft,
    AircraftRollRight,
    AircraftTargetLeft,
    AircraftTargetRight,
}

/// Explicitly selected control profile.  The native host must choose this
/// profile; this module never guesses from vehicle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualProfile {
    Walking,
    Car,
    Aircraft,
}

impl ManualProfile {
    pub const fn binding(self, action: TouchAction) -> Option<Binding> {
        use Binding::{
            Chord, ContextMenu, Key, MouseAxis, MouseButton as MouseBinding, MouseWheel,
            RadialMenu,
        };
        use TouchAction::*;

        match action {
            MoveForward => Some(Key(VK_W)),
            MoveBackward => Some(Key(VK_S)),
            MoveLeft => Some(Key(VK_A)),
            MoveRight => Some(Key(VK_D)),
            Camera => Some(MouseAxis),
            Sprint => match self {
                Self::Walking => Some(Key(VK_SHIFT_LEFT)),
                Self::Car | Self::Aircraft => None,
            },
            Jump => match self {
                Self::Walking => Some(Key(VK_SPACE)),
                Self::Car | Self::Aircraft => None,
            },
            Melee => match self {
                Self::Walking => Some(Key(VK_R)),
                Self::Car | Self::Aircraft => None,
            },
            EnterExit => match self {
                Self::Walking | Self::Car => Some(Key(VK_F)),
                Self::Aircraft => None,
            },
            Fire => match self {
                Self::Walking => Some(MouseBinding(crate::MouseButton::Left)),
                Self::Car | Self::Aircraft => None,
            },
            Aim => match self {
                Self::Walking => Some(MouseBinding(crate::MouseButton::Right)),
                Self::Car | Self::Aircraft => None,
            },
            Cover => match self {
                Self::Walking => Some(Key(VK_Q)),
                Self::Car | Self::Aircraft => None,
            },
            Weapons => match self {
                Self::Walking | Self::Car => Some(Key(VK_TAB)),
                Self::Aircraft => None,
            },
            Gas => match self {
                Self::Car | Self::Aircraft => Some(Key(VK_W)),
                Self::Walking => None,
            },
            Brake => match self {
                Self::Car | Self::Aircraft => Some(Key(VK_S)),
                Self::Walking => None,
            },
            Handbrake => match self {
                Self::Car => Some(Key(VK_SPACE)),
                Self::Walking | Self::Aircraft => None,
            },
            Shoot => match self {
                Self::Car => Some(MouseBinding(crate::MouseButton::Left)),
                Self::Walking | Self::Aircraft => None,
            },
            Horn => match self {
                Self::Car => Some(Key(VK_E)),
                Self::Walking | Self::Aircraft => None,
            },
            Radio => match self {
                Self::Car => Some(Key(VK_Q)),
                Self::Walking | Self::Aircraft => None,
            },
            Lights => match self {
                Self::Car => Some(Key(VK_H)),
                Self::Walking | Self::Aircraft => None,
            },
            LookBack => match self {
                Self::Walking | Self::Car => Some(Key(VK_C)),
                Self::Aircraft => None,
            },
            CameraView => match self {
                Self::Walking | Self::Car => Some(Key(VK_V)),
                Self::Aircraft => None,
            },
            SwitchCharacter => match self {
                Self::Walking | Self::Car => Some(Key(VK_ALT_LEFT)),
                Self::Aircraft => None,
            },
            Special => match self {
                Self::Walking | Self::Car => Some(Key(VK_CAPS_LOCK)),
                Self::Aircraft => None,
            },
            Reload => match self {
                Self::Walking => Some(Key(VK_R)),
                Self::Car | Self::Aircraft => None,
            },
            NextWeapon => match self {
                Self::Walking | Self::Car => Some(MouseWheel(-1)),
                Self::Aircraft => None,
            },
            Pause => match self {
                Self::Walking | Self::Car => Some(Key(VK_ESCAPE)),
                Self::Aircraft => None,
            },
            Map => match self {
                Self::Walking | Self::Car => Some(Key(VK_P)),
                Self::Aircraft => None,
            },
            MenuSelect => Some(Key(VK_ENTER)),
            MenuBack => Some(Key(VK_BACKSPACE)),
            MenuTabPrevious => Some(Key(VK_Q)),
            MenuTabNext => Some(Key(VK_E)),
            MenuApply => Some(Key(VK_SPACE)),
            MenuDelete => Some(Key(VK_DELETE)),
            DebugKeyboardMode => Some(Chord(VK_SHIFT_LEFT, VK_P)),
            DpadUp => Some(Key(VK_ARROW_UP)),
            DpadDown => Some(Key(VK_ARROW_DOWN)),
            DpadLeft => Some(Key(VK_ARROW_LEFT)),
            DpadRight => Some(Key(VK_ARROW_RIGHT)),
            FaceSouth => Some(Key(VK_SPACE)),
            FaceEast => Some(Key(VK_F)),
            FaceWest => Some(Key(VK_R)),
            FaceNorth => Some(Key(VK_Y)),
            LeftBumper => Some(Key(VK_Q)),
            RightBumper => Some(Key(VK_E)),
            LeftTrigger => Some(MouseBinding(crate::MouseButton::Right)),
            RightTrigger => Some(MouseBinding(crate::MouseButton::Left)),
            LeftStickPress => Some(Key(VK_SHIFT_LEFT)),
            RightStickPress => Some(Key(VK_V)),
            Start => Some(Key(VK_ENTER)),
            Back => Some(Key(VK_ESCAPE)),
            Context => Some(ContextMenu),
            Radial => Some(RadialMenu),
            AircraftPitchUp => Some(Key(VK_NUMPAD_8)),
            AircraftPitchDown => Some(Key(VK_NUMPAD_2)),
            AircraftRollLeft => Some(Key(VK_NUMPAD_4)),
            AircraftRollRight => Some(Key(VK_NUMPAD_6)),
            AircraftTargetLeft => Some(Key(VK_NUMPAD_7)),
            AircraftTargetRight => Some(Key(VK_NUMPAD_9)),
        }
    }
}

/// Normalized placement for the compact touch surface.  The host can scale
/// these values to any S24 orientation and keep the game view unobstructed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormalizedRect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl NormalizedRect {
    pub const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }
}

/// Compact handheld layout metadata.  It describes controls only; it does not
/// draw a UI or run an Android/browser event loop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactTouchLayout {
    pub left_stick: NormalizedRect,
    pub right_stick: NormalizedRect,
    pub dpad: NormalizedRect,
    pub face_buttons: NormalizedRect,
    pub shoulders: NormalizedRect,
    pub menu: NormalizedRect,
}

impl Default for CompactTouchLayout {
    fn default() -> Self {
        Self {
            left_stick: NormalizedRect::new(0.03, 0.62, 0.27, 0.94),
            right_stick: NormalizedRect::new(0.73, 0.62, 0.97, 0.94),
            dpad: NormalizedRect::new(0.03, 0.30, 0.24, 0.50),
            face_buttons: NormalizedRect::new(0.76, 0.30, 0.97, 0.50),
            shoulders: NormalizedRect::new(0.03, 0.04, 0.97, 0.18),
            menu: NormalizedRect::new(0.39, 0.38, 0.61, 0.62),
        }
    }
}


