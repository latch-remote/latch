//! Recover a cursor for composition when duplication cannot supply its shape.
//! GetIconInfo copies its bitmaps, so a cursor changing during the read cannot
//! invalidate them. No display pixels are read and no desktop state is changed.
use anyhow::{ensure, Context, Result};
use windows::Win32::{
    Graphics::{Dxgi::DXGI_OUTDUPL_POINTER_SHAPE_INFO, Gdi::*},
    UI::WindowsAndMessaging::*,
};

struct Icon(ICONINFO);
impl Drop for Icon {
    fn drop(&mut self) {
        // SAFETY: GetIconInfo transfers ownership of these two bitmap copies.
        unsafe {
            if !self.0.hbmColor.is_invalid() {
                let _ = DeleteObject(self.0.hbmColor);
            }
            if !self.0.hbmMask.is_invalid() {
                let _ = DeleteObject(self.0.hbmMask);
            }
        }
    }
}
fn bitmap(bitmap: HBITMAP) -> Result<(u32, u32, Vec<u8>)> {
    let mut object = BITMAP::default();
    // SAFETY: object is a writable BITMAP and the handle belongs to Icon.
    ensure!(
        unsafe {
            GetObjectW(
                bitmap,
                std::mem::size_of::<BITMAP>() as i32,
                Some((&mut object as *mut BITMAP).cast()),
            )
        } != 0,
        "read cursor bitmap dimensions"
    );
    ensure!(
        object.bmWidth > 0
            && object.bmHeight > 0
            && object.bmWidth <= 4096
            && object.bmHeight <= 8192,
        "invalid cursor bitmap dimensions"
    );
    let (width, height) = (object.bmWidth as u32, object.bmHeight as u32);
    let mut bytes = vec![0; (width * height * 4) as usize];
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: object.bmWidth,
            biHeight: -object.bmHeight,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: The desktop DC is borrowed only for format conversion. The DIB
    // buffer covers every requested scanline and ReleaseDC balances GetDC.
    let copied = unsafe {
        let dc = GetDC(None);
        ensure!(!dc.is_invalid(), "get cursor conversion DC");
        let copied = GetDIBits(
            dc,
            bitmap,
            0,
            height,
            Some(bytes.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, dc);
        copied
    };
    ensure!(copied == height as i32, "read cursor bitmap pixels");
    Ok((width, height, bytes))
}
pub(super) fn system_shape(cursor: HCURSOR) -> Result<(DXGI_OUTDUPL_POINTER_SHAPE_INFO, Vec<u8>)> {
    let mut icon = Icon(ICONINFO::default());
    // SAFETY: A system-owned cursor is borrowed, never destroyed. Icon owns
    // only the copies returned by GetIconInfo, including on subsequent errors.
    unsafe { GetIconInfo(HICON(cursor.0), &mut icon.0) }.context("copy system cursor")?;
    let (width, mask_height, mask) = bitmap(icon.0.hbmMask)?;
    let (height, mut bytes) = if icon.0.hbmColor.is_invalid() {
        ensure!(
            mask_height.is_multiple_of(2),
            "odd monochrome cursor mask height"
        );
        (
            mask_height / 2,
            mask[(width * (mask_height / 2) * 4) as usize..].to_vec(),
        )
    } else {
        let (w, h, color) = bitmap(icon.0.hbmColor)?;
        ensure!(w == width && h == mask_height, "mismatched cursor bitmaps");
        (h, color)
    };
    let alpha =
        !icon.0.hbmColor.is_invalid() && bytes.as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 0);
    if !alpha {
        for (pixel, mask) in bytes
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(mask.as_chunks::<4>().0.iter())
        {
            pixel[3] = mask[0];
        }
    }
    Ok((
        DXGI_OUTDUPL_POINTER_SHAPE_INFO {
            Type: if alpha { 2 } else { 4 },
            Width: width,
            Height: height,
            Pitch: width * 4,
            HotSpot: windows::Win32::Foundation::POINT {
                x: icon.0.xHotspot as i32,
                y: icon.0.yHotspot as i32,
            },
        },
        bytes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_system_arrow_is_readable_for_composition() -> Result<()> {
        // SAFETY: LoadCursorW borrows the built-in shared resource. This test
        // reads its copied bitmaps without changing the desktop or cursor.
        let arrow = unsafe { LoadCursorW(None, IDC_ARROW) }?;
        let (shape, bytes) = system_shape(arrow)?;
        let metadata = crate::windows_cursor::ShapeInfo {
            kind: shape.Type,
            width: shape.Width,
            height: shape.Height,
            pitch: shape.Pitch,
            hot_x: shape.HotSpot.x,
            hot_y: shape.HotSpot.y,
        };
        let pixels = metadata
            .pixels(&bytes)
            .context("invalid recovered system arrow")?;
        ensure!(!pixels.is_empty(), "system arrow is empty");
        Ok(())
    }
}
