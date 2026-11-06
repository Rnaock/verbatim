//! Capture de la fenêtre de Verbatim (pour signaler un problème) et copie de
//! l'image dans le presse-papiers, prête à coller dans GitHub.
#![cfg(windows)]

use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_DIB;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

/// Image BGRA, lignes de haut en bas.
pub struct Shot {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Capture le contenu de la fenêtre, WebView comprise (PW_RENDERFULLCONTENT).
pub fn capture(hwnd: HWND) -> Result<Shot, String> {
    unsafe {
        let mut rc = RECT::default();
        GetClientRect(hwnd, &mut rc).map_err(|e| e.message())?;
        let (w, h) = ((rc.right - rc.left).max(1), (rc.bottom - rc.top).max(1));
        let wdc = GetDC(Some(hwnd));
        let mdc = CreateCompatibleDC(Some(wdc));
        let bmp = CreateCompatibleBitmap(wdc, w, h);
        let old = SelectObject(mdc, bmp.into());
        // 1 = PW_CLIENTONLY, 2 = PW_RENDERFULLCONTENT (nécessaire pour WebView2).
        let ok = PrintWindow(hwnd, mdc, PRINT_WINDOW_FLAGS(3)).as_bool();
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // de haut en bas
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(mdc, bmp, 0, h as u32, Some(buf.as_mut_ptr().cast()), &mut bmi, DIB_RGB_COLORS);
        SelectObject(mdc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mdc);
        ReleaseDC(Some(hwnd), wdc);
        if !ok || lines == 0 {
            return Err("capture de la fenêtre impossible".into());
        }
        for px in buf.chunks_exact_mut(4) {
            px[3] = 255; // opaque
        }
        Ok(Shot { width: w as u32, height: h as u32, bgra: buf })
    }
}

/// Encode en PNG (RGBA).
pub fn png(shot: &Shot) -> Result<Vec<u8>, String> {
    let mut rgba = shot.bgra.clone();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, shot.width, shot.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().map_err(|e| e.to_string())?;
        wr.write_image_data(&rgba).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn global(bytes: &[u8]) -> Result<HANDLE, String> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(|e| e.message())?;
        let p = GlobalLock(h);
        if p.is_null() {
            return Err("mémoire indisponible".into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(h);
        Ok(HANDLE(h.0))
    }
}

/// Copie l'image dans le presse-papiers : format « PNG » (navigateurs) et
/// bitmap Windows (autres applications).
pub fn to_clipboard(shot: &Shot, png_bytes: &[u8]) -> Result<(), String> {
    // CF_DIB : en-tête + pixels de bas en haut.
    let header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: shot.width as i32,
        biHeight: shot.height as i32,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: shot.width * shot.height * 4,
        ..Default::default()
    };
    let mut dib = Vec::with_capacity(std::mem::size_of::<BITMAPINFOHEADER>() + shot.bgra.len());
    dib.extend_from_slice(unsafe {
        std::slice::from_raw_parts((&header as *const BITMAPINFOHEADER).cast::<u8>(), std::mem::size_of::<BITMAPINFOHEADER>())
    });
    let row = (shot.width * 4) as usize;
    for y in (0..shot.height as usize).rev() {
        dib.extend_from_slice(&shot.bgra[y * row..(y + 1) * row]);
    }
    unsafe {
        OpenClipboard(None).map_err(|e| e.message())?;
        let res = (|| -> Result<(), String> {
            EmptyClipboard().map_err(|e| e.message())?;
            let png_fmt = RegisterClipboardFormatW(w!("PNG"));
            if png_fmt != 0 {
                let h = global(png_bytes)?;
                SetClipboardData(png_fmt, Some(h)).map_err(|e| e.message())?;
            }
            let h = global(&dib)?;
            SetClipboardData(CF_DIB.0 as u32, Some(h)).map_err(|e| e.message())?;
            Ok(())
        })();
        let _ = CloseClipboard();
        res
    }
}
