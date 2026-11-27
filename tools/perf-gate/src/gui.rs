//! Rust port of `scripts/gui-smoke.ps1` + its scenarios: launch a dev-hooks
//! release in a fresh profile, drive it with BAYAN_* hooks or a real click,
//! capture the PID-owned HWND with a DPI-aware PrintWindow, and assert
//! pixels. Only the spawned child is killed; the user's ~/.bayan is untouched.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type HWND = isize;
type HANDLE = isize;
type BOOL = i32;
type HDC = isize;
type HBITMAP = isize;
type HGDIOBJ = isize;

#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}
#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}
#[repr(C)]
struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels: i32,
    bi_y_pels: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

#[link(name = "user32")]
extern "system" {
    fn SetProcessDpiAwarenessContext(v: isize) -> BOOL;
    fn EnumWindows(cb: extern "system" fn(HWND, isize) -> BOOL, l: isize) -> BOOL;
    fn GetWindowThreadProcessId(h: HWND, pid: *mut u32) -> u32;
    fn IsWindowVisible(h: HWND) -> BOOL;
    fn GetClientRect(h: HWND, r: *mut Rect) -> BOOL;
    fn ClientToScreen(h: HWND, p: *mut Point) -> BOOL;
    fn SetCursorPos(x: i32, y: i32) -> BOOL;
    fn mouse_event(flags: u32, dx: u32, dy: u32, data: u32, extra: usize);
    fn SetForegroundWindow(h: HWND) -> BOOL;
    fn PrintWindow(h: HWND, hdc: HDC, flags: u32) -> BOOL;
    fn GetDpiForWindow(h: HWND) -> u32;
    fn GetDC(h: HWND) -> HDC;
    fn ReleaseDC(h: HWND, hdc: HDC) -> i32;
}
#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(hdc: HDC) -> HDC;
    fn DeleteDC(hdc: HDC) -> BOOL;
    fn CreateCompatibleBitmap(hdc: HDC, w: i32, h: i32) -> HBITMAP;
    fn SelectObject(hdc: HDC, obj: HGDIOBJ) -> HGDIOBJ;
    fn DeleteObject(obj: HGDIOBJ) -> BOOL;
    fn GetDIBits(hdc: HDC, bmp: HBITMAP, start: u32, lines: u32, bits: *mut u8, bmi: *mut BitmapInfoHeader, usage: u32) -> i32;
}

struct Bmp {
    w: usize,
    h: usize,
    /// BGRA, top-down.
    px: Vec<u8>,
}
impl Bmp {
    fn pixel(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let i = (y * self.w + x) * 4;
        (self.px[i + 2], self.px[i + 1], self.px[i])
    }
    fn ink_count(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> usize {
        let (br, bg, bb) = self.pixel(x0, y0);
        let mut n = 0;
        let mut y = y0;
        while y <= y1 {
            let mut x = x0;
            while x <= x1 {
                let (r, g, b) = self.pixel(x, y);
                if (r as i32 - br as i32).abs() > 12 || (g as i32 - bg as i32).abs() > 12 || (b as i32 - bb as i32).abs() > 12 {
                    n += 1;
                }
                x += 2;
            }
            y += 2;
        }
        n
    }
    fn uniform(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> bool {
        self.ink_count(x0, y0, x1, y1) == 0
    }
}

fn capture(hwnd: HWND) -> Result<(Bmp, u32), String> {
    unsafe {
        let mut r = Rect { left: 0, top: 0, right: 0, bottom: 0 };
        GetClientRect(hwnd, &mut r);
        let w = (r.right - r.left) as usize;
        let h = (r.bottom - r.top) as usize;
        if w < 100 || h < 100 {
            return Err(format!("client rect too small: {w}x{h}"));
        }
        let screen = GetDC(0);
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w as i32, h as i32);
        let old = SelectObject(mem, bmp);
        let ok = PrintWindow(hwnd, mem, 2);
        let mut px = vec![0u8; w * h * 4];
        let mut bmi: BitmapInfoHeader = std::mem::zeroed();
        bmi.bi_size = std::mem::size_of::<BitmapInfoHeader>() as u32;
        bmi.bi_width = w as i32;
        bmi.bi_height = -(h as i32); // top-down
        bmi.bi_planes = 1;
        bmi.bi_bit_count = 32;
        bmi.bi_compression = 0;
        GetDIBits(mem, bmp, 0, h as u32, px.as_mut_ptr(), &mut bmi, 0);
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(0, screen);
        if ok == 0 {
            return Err("PrintWindow refused the capture".into());
        }
        let mut dpi = GetDpiForWindow(hwnd);
        if dpi < 96 {
            dpi = 96;
        }
        Ok((Bmp { w, h, px }, dpi))
    }
}

fn write_bmp(path: &Path, b: &Bmp) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = fs::File::create(path)?;
    let off = 14 + 40;
    let size = off + b.px.len();
    f.write_all(b"BM")?;
    f.write_all(&(size as u32).to_le_bytes())?;
    f.write_all(&[0u8; 4])?;
    f.write_all(&(off as u32).to_le_bytes())?;
    f.write_all(&40u32.to_le_bytes())?;
    f.write_all(&(b.w as i32).to_le_bytes())?;
    f.write_all(&(-(b.h as i32)).to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&32u16.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&(b.px.len() as u32).to_le_bytes())?;
    f.write_all(&[0u8; 16])?;
    f.write_all(&b.px)?;
    Ok(())
}

struct FindCtx {
    pid: u32,
    found: HWND,
}
extern "system" fn find_cb(h: HWND, l: isize) -> BOOL {
    unsafe {
        let ctx = &mut *(l as *mut FindCtx);
        let mut pid = 0;
        GetWindowThreadProcessId(h, &mut pid);
        if pid == ctx.pid && IsWindowVisible(h) != 0 {
            ctx.found = h;
            return 0;
        }
    }
    1
}
fn find_window(pid: u32) -> HWND {
    let mut ctx = FindCtx { pid, found: 0 };
    unsafe { EnumWindows(find_cb, &mut ctx as *mut FindCtx as isize) };
    ctx.found
}

fn click(hwnd: HWND, bmp_w: usize, bmp_h: usize, fx: f64, fy: f64, right: bool) {
    unsafe {
        let mut p = Point { x: (bmp_w as f64 * fx) as i32, y: (bmp_h as f64 * fy) as i32 };
        ClientToScreen(hwnd, &mut p);
        SetCursorPos(p.x, p.y);
        std::thread::sleep(Duration::from_millis(250));
        let (down, up) = if right { (0x0008, 0x0010) } else { (0x0002, 0x0004) };
        mouse_event(down, 0, 0, 0, 0);
        std::thread::sleep(Duration::from_millis(100));
        mouse_event(up, 0, 0, 0, 0);
    }
}

struct Scenario {
    name: &'static str,
    env: Vec<(&'static str, String)>,
    settle: u64,
    ci_safe: bool,
    click: Option<(f64, f64, bool)>,
    assert: Box<dyn Fn(&Bmp, u32) -> Result<(), String>>,
}

fn band(b: &Bmp, dpi: u32, x0: f64, y0: f64, x1: f64, y1: f64) -> (usize, usize, usize, usize) {
    let _ = dpi;
    (
        (b.w as f64 * x0) as usize,
        (b.h as f64 * y0) as usize,
        (b.w as f64 * x1) as usize,
        (b.h as f64 * y1) as usize,
    )
}

fn assert_ink_region(x0: f64, y0: f64, x1: f64, y1: f64) -> impl Fn(&Bmp, u32) -> Result<(), String> {
    move |b, dpi| {
        let (a, c, d, e) = band(b, dpi, x0, y0, x1, y1);
        if b.uniform(a, c, d, e) {
            Err(format!("region ({a},{c})-({d},{e}) is uniform: nothing drawn"))
        } else {
            Ok(())
        }
    }
}

fn red_border_check(b: &Bmp) -> Result<(), String> {
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for y in 0..b.h {
        for x in 0..b.w {
            let (r, g, bl) = b.pixel(x, y);
            if (r as i32 - 255).abs() <= 4 && g <= 4 && bl <= 4 {
                hits.push((x, y));
            }
        }
    }
    if hits.len() < 30 {
        return Err(format!("only {} red pixels: the borders never drew", hits.len()));
    }
    hits.sort_by_key(|&(x, y)| (y, x));
    let mut bands: Vec<usize> = Vec::new();
    let mut cur: Option<(usize, usize)> = None; // (y1, minx)
    for (x, y) in hits {
        match cur {
            Some((y1, mn)) if y <= y1 + 3 => cur = Some((y, mn.min(x))),
            Some((_, mn)) => {
                bands.push(mn);
                cur = Some((y, x));
            }
            None => cur = Some((y, x)),
        }
    }
    if let Some((_, mn)) = cur {
        bands.push(mn);
    }
    if bands.len() != 3 {
        return Err(format!("expected 3 border bands, found {}", bands.len()));
    }
    let mn = *bands.iter().min().unwrap();
    let mx = *bands.iter().max().unwrap();
    if mx - mn > 2 {
        return Err(format!("border columns disagree: x = {bands:?}"));
    }
    Ok(())
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "selftest-red",
            env: vec![("BAYAN_TYPE", "echo \"$([char]27)[48;2;255;0;0m$([char]27)[2J\"\r".into())],
            settle: 10,
            ci_safe: false,
            click: None,
            assert: Box::new(|b, dpi| {
                let top = (60.0 * dpi as f64 / 96.0) as usize;
                let gh = b.h - top;
                for (fx, fy) in [(0.25, 0.30), (0.50, 0.50), (0.75, 0.70)] {
                    let x = (b.w as f64 * fx) as usize;
                    let y = top + (gh as f64 * fy) as usize;
                    let (r, g, bl) = b.pixel(x, y);
                    if (r as i32 - 255).abs() > 2 || g > 2 || bl > 2 {
                        return Err(format!("pixel ({x},{y}) = ({r},{g},{bl}), want (255,0,0) +/-2"));
                    }
                }
                Ok(())
            }),
        },
        Scenario {
            name: "arabic-connected",
            env: vec![("BAYAN_TYPE", "Write-Host \"$([char]27)[12;10Hمرحبا بالعالم هذا نثر\"\r".into())],
            settle: 10,
            ci_safe: false,
            click: None,
            assert: Box::new(|b, dpi| {
                let (a, c, d, e) = band(b, dpi, 0.05, 0.62, 0.95, 0.67);
                let n = b.ink_count(a, c, d, e);
                if n < 100 {
                    Err(format!("Arabic line drew only {n} ink samples (want >= 100)"))
                } else {
                    Ok(())
                }
            }),
        },
        Scenario {
            name: "tui-border-columns",
            env: vec![("BAYAN_TYPE", "Write-Host \"$([char]27)[10;1H│ 1. Trust folder$([char]27)[10;41H$([char]27)[38;2;255;0;0m│$([char]27)[0m\"; Write-Host \"$([char]27)[13;1H│ 2. (سلام abc مرحبا)$([char]27)[13;41H$([char]27)[38;2;255;0;0m│$([char]27)[0m\"; Write-Host \"$([char]27)[16;1H│ 3. plain ascii row$([char]27)[16;41H$([char]27)[38;2;255;0;0m│$([char]27)[0m\"\r".into())],
            settle: 10,
            ci_safe: false,
            click: None,
            assert: Box::new(|b, _dpi| red_border_check(b)),
        },
        Scenario {
            name: "cjk-columns",
            env: vec![("BAYAN_TYPE", "Write-Host \"$([char]27)[10;1H│ 1. Trust folder$([char]27)[10;41H$([char]27)[38;2;255;0;0m│$([char]27)[0m\"; Write-Host \"$([char]27)[13;1H│ 2. (你好世界 テスト 한국어)$([char]27)[13;41H$([char]27)[38;2;255;0;0m│$([char]27)[0m\"; Write-Host \"$([char]27)[16;1H│ 3. plain ascii row$([char]27)[16;41H$([char]27)[38;2;255;0;0m│$([char]27)[0m\"\r".into())],
            settle: 10,
            ci_safe: false,
            click: None,
            assert: Box::new(|b, _dpi| red_border_check(b)),
        },
        Scenario {
            name: "rtl-flush-right",
            env: vec![("BAYAN_TYPE", "echo مرحبا بالعالم هذا نثر طويل يكفي\r".into())],
            settle: 10,
            ci_safe: true,
            click: None,
            assert: Box::new(|b, _dpi| {
                // find an ink row-band flush right with nothing at left
                let (br, bg, bb) = b.pixel(2, (b.h as f64 * 0.95) as usize);
                let mut y = (b.h as f64 * 0.08) as usize;
                let mut cur: Option<(usize, usize)> = None;
                let mut bands: Vec<(usize, usize)> = Vec::new();
                while y < (b.h as f64 * 0.60) as usize {
                    let mut minx = None;
                    let mut maxx = 0;
                    let mut x = 2;
                    while x < b.w - 2 {
                        let (r, g, bl) = b.pixel(x, y);
                        if (r as i32 - br as i32).abs() > 12 || (g as i32 - bg as i32).abs() > 12 || (bl as i32 - bb as i32).abs() > 12 {
                            minx.get_or_insert(x);
                            maxx = x;
                        }
                        x += 2;
                    }
                    if let Some(mn) = minx {
                        match cur {
                            None => cur = Some((mn, maxx)),
                            Some((a, c)) => cur = Some((a.min(mn), c.max(maxx))),
                        }
                    } else if let Some(band) = cur.take() {
                        bands.push(band);
                    }
                    y += 2;
                }
                if let Some(band) = cur {
                    bands.push(band);
                }
                for (mn, mx) in bands {
                    if mn > (b.w as f64 * 0.55) as usize && mx > (b.w as f64 * 0.96) as usize {
                        return Ok(());
                    }
                }
                Err("no RTL flush-right row band".into())
            }),
        },
        Scenario {
            name: "cyrillic-ltr",
            env: vec![("BAYAN_TYPE", "echo Привет мир это длинная строка текста\r".into())],
            settle: 10,
            ci_safe: true,
            click: None,
            assert: Box::new(|b, _dpi| {
                let (br, bg, bb) = b.pixel(2, (b.h as f64 * 0.95) as usize);
                let mut y = (b.h as f64 * 0.08) as usize;
                let mut cur: Option<(usize, usize)> = None;
                let mut bands: Vec<(usize, usize)> = Vec::new();
                while y < (b.h as f64 * 0.60) as usize {
                    let mut minx = None;
                    let mut maxx = 0;
                    let mut x = 2;
                    while x < b.w - 2 {
                        let (r, g, bl) = b.pixel(x, y);
                        if (r as i32 - br as i32).abs() > 12 || (g as i32 - bg as i32).abs() > 12 || (bl as i32 - bb as i32).abs() > 12 {
                            minx.get_or_insert(x);
                            maxx = x;
                        }
                        x += 2;
                    }
                    if let Some(mn) = minx {
                        match cur {
                            None => cur = Some((mn, maxx)),
                            Some((a, c)) => cur = Some((a.min(mn), c.max(maxx))),
                        }
                    } else if let Some(band) = cur.take() {
                        bands.push(band);
                    }
                    y += 2;
                }
                if let Some(band) = cur {
                    bands.push(band);
                }
                for (mn, mx) in bands {
                    if mn < (b.w as f64 * 0.10) as usize && mx < (b.w as f64 * 0.60) as usize {
                        return Ok(());
                    }
                }
                Err("no LTR left-aligned Cyrillic band".into())
            }),
        },
        Scenario {
            name: "context-menu",
            env: vec![],
            settle: 4,
            ci_safe: true,
            click: Some((0.5, 0.5, true)),
            assert: Box::new(assert_ink_region(0.55, 0.30, 0.95, 0.80)),
        },
        Scenario {
            name: "palette",
            env: vec![("BAYAN_PALETTE", "1".into())],
            settle: 3,
            ci_safe: true,
            click: None,
            assert: Box::new(assert_ink_region(0.30, 0.25, 0.70, 0.60)),
        },
        Scenario {
            name: "guard-card",
            env: vec![("BAYAN_GUARD", "1".into())],
            settle: 3,
            ci_safe: true,
            click: None,
            assert: Box::new(assert_ink_region(0.30, 0.25, 0.70, 0.60)),
        },
        Scenario {
            name: "settings-english",
            env: vec![("BAYAN_SETTINGS", "1".into()), ("BAYAN_LANG", "en".into())],
            settle: 3,
            ci_safe: true,
            click: None,
            assert: Box::new(assert_ink_region(0.20, 0.15, 0.80, 0.80)),
        },
        Scenario {
            name: "atlas-stress",
            env: vec![("BAYAN_ATLAS_STRESS", "1".into())],
            settle: 4,
            ci_safe: true,
            click: None,
            assert: Box::new(assert_ink_region(0.10, 0.20, 0.90, 0.80)),
        },
        Scenario {
            // A1: Kitty graphics (t=d raw RGB) must survive the pinned ConPTY
            // host's VT passthrough and land as pixels. Requires the pinned
            // conhost beside the binary (inbox conhost strips APC).
            name: "kitty-image",
            env: vec![("BAYAN_TYPE", "Write-Host \"$([char]27)_Ga=T,f=24,s=2,v=2,i=1;/wAA/wAA/wAA/wAA$([char]27)\\\"\r".into())],
            settle: 6,
            ci_safe: false,
            click: None,
            assert: Box::new(|b, _dpi| {
                let mut n = 0;
                for y in 0..b.h {
                    for x in 0..b.w {
                        let (r, g, bl) = b.pixel(x, y);
                        if (r as i32 - 255).abs() <= 4 && g <= 4 && bl <= 4 {
                            n += 1;
                        }
                    }
                }
                if n < 4 {
                    Err(format!("kitty image drew only {n} red pixels (want >= 4); APC not delivered?"))
                } else {
                    Ok(())
                }
            }),
        },
    ]
}

pub fn run(args: &[String]) -> i32 {
    unsafe { SetProcessDpiAwarenessContext(-4) };
    let binary = args.windows(2).find(|w| w[0] == "--binary").map(|w| w[1].clone()).unwrap_or_else(|| "target/release/bayan.exe".into());
    let only = args.windows(2).find(|w| w[0] == "--scenario").map(|w| w[1].clone());
    let ci_safe = args.iter().any(|a| a == "--ci-safe");
    let exe = match fs::canonicalize(&binary) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("::error::{e}");
            return 2;
        }
    };
    let mut all_pass = true;
    for sc in scenarios() {
        if let Some(o) = &only {
            if o != sc.name {
                continue;
            }
        }
        if ci_safe && !sc.ci_safe {
            continue;
        }
        let ok = run_scenario(&exe, &sc);
        if !ok {
            all_pass = false;
        }
    }
    if all_pass {
        0
    } else {
        1
    }
}

fn run_scenario(exe: &Path, sc: &Scenario) -> bool {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("bayan-smoke-{nanos:x}"));
    let home = root.join("home");
    let temp = root.join("temp");
    let result = (|| -> Result<(), String> {
        fs::create_dir_all(home.join(".bayan")).and_then(|_| fs::create_dir_all(&temp)).map_err(|e| e.to_string())?;
        fs::write(home.join(".bayan").join("config.json"), "{\"font_family\":\"Kawkab Mono\",\"font_size\":16.0,\"padding\":0,\"opacity\":1.0,\"cursor_blink\":false,\"shell\":\"powershell.exe\",\"scroll\":{\"smooth\":false}}").map_err(|e| e.to_string())?;
        fs::write(home.join(".bayan").join("session.json"), "{\"tabs\":[],\"active\":0,\"window\":{\"x\":50,\"y\":50,\"w\":1000,\"h\":700,\"maximized\":false}}").map_err(|e| e.to_string())?;
        let log = temp.join("bayan_profile.log");
        let stderr_path = root.join("stderr.txt");
        let stderr_file = fs::File::create(&stderr_path).map_err(|e| e.to_string())?;
        let mut cmd = Command::new(exe);
        cmd.env("USERPROFILE", &home)
            .env("HOME", &home)
            .env("TEMP", &temp)
            .env("TMP", &temp)
            .env("BAYAN_PROFILE", "1")
            .env("BAYAN_SHOW_NOW", "1")
            .stderr(Stdio::from(stderr_file));
        for (k, v) in &sc.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut ready = false;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return Err("Bayan exited before capture".into());
            }
            if let Ok(t) = fs::read_to_string(&log) {
                if t.contains(" ms  renderer ready") {
                    ready = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if !ready {
            let _ = child.kill();
            let _ = child.wait();
            return Err("window never reached renderer ready".into());
        }
        std::thread::sleep(Duration::from_secs(sc.settle));
        let pid = child.id();
        let hwnd = find_window(pid);
        if hwnd == 0 {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("no visible window owned by PID {pid}"));
        }
        if let Some((fx, fy, right)) = sc.click {
            unsafe { SetForegroundWindow(hwnd) };
            std::thread::sleep(Duration::from_millis(400));
            let mut r = Rect { left: 0, top: 0, right: 0, bottom: 0 };
            unsafe { GetClientRect(hwnd, &mut r) };
            click(hwnd, (r.right - r.left) as usize, (r.bottom - r.top) as usize, fx, fy, right);
            std::thread::sleep(Duration::from_millis(1200));
        }
        let (bmp, dpi) = capture(hwnd)?;
        let _ = child.kill();
        let _ = child.wait();
        let res = (sc.assert)(&bmp, dpi);
        if res.is_err() {
            let _ = write_bmp(&root.join(format!("{}.bmp", sc.name)), &bmp);
        }
        res
    })();
    if result.is_ok() {
        let _ = fs::remove_dir_all(&root);
    }
    match result {
        Ok(()) => {
            println!("PASS {}", sc.name);
            true
        }
        Err(e) => {
            println!("FAIL {}: {e} (artifacts: {})", sc.name, root.display());
            false
        }
    }
}
