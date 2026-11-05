//! Stand-in for `shared_input.rs` in builds without the input adapter
//! (it is built for x64 MSVC only): attaching always fails.

use anyhow::Result;
use std::path::Path;
use windows::Win32::Foundation::HWND;

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub active: bool,
    pub error: u32,
    pub cursor: Option<SharedCursor>,
    pub image_generation: u64,
}

#[derive(Clone, Debug)]
pub struct SharedCursor {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub visible: bool,
    pub rgba: Vec<u8>,
}

pub struct SharedInput(());

impl SharedInput {
    pub fn attach(_hwnd: HWND, _dir: &Path) -> Result<Self> {
        anyhow::bail!("isolated input is not in this build (x64 MSVC builds only)")
    }
    pub fn set_relative(&mut self, _relative: bool) {}
    pub fn key(&mut self, _scan: u16, _down: bool) -> Result<()> {
        Ok(())
    }
    pub fn move_to(&mut self, _x: u16, _y: u16) -> Result<()> {
        Ok(())
    }
    pub fn motion(&mut self, _dx: i32, _dy: i32) -> Result<()> {
        Ok(())
    }
    pub fn button(&mut self, _button: i32, _down: bool, _x: u16, _y: u16) -> Result<()> {
        Ok(())
    }
    pub fn scroll(&mut self, _dx: i32, _dy: i32) -> Result<()> {
        Ok(())
    }
    pub fn release(&mut self) -> Result<()> {
        Ok(())
    }
    pub fn heartbeat(&mut self) -> Result<()> {
        Ok(())
    }
    pub fn resume(&mut self) -> Result<()> {
        Ok(())
    }
    pub fn status(&mut self) -> Result<Status> {
        Ok(Status::default())
    }
}
