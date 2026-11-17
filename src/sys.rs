//! Switchboard's own footprint, shown on the page so its cost is visible.

use std::time::Instant;

pub struct Sys {
    last_cpu: f64,
    last_wall: Instant,
    pub started: Instant,
    pub cpu_pct: f64,
}

fn cpu_seconds() -> f64 {
    // SAFETY: getrusage fills a struct we own.
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut ru) != 0 {
            return 0.0;
        }
        let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
        t(ru.ru_utime) + t(ru.ru_stime)
    }
}

#[cfg(target_os = "macos")]
mod mac {
    /// `struct rusage_info_v2` from <sys/resource.h>.
    #[repr(C)]
    #[derive(Default)]
    pub struct RusageInfoV2 {
        pub ri_uuid: [u8; 16],
        pub ri_user_time: u64,
        pub ri_system_time: u64,
        pub ri_pkg_idle_wkups: u64,
        pub ri_interrupt_wkups: u64,
        pub ri_pageins: u64,
        pub ri_wired_size: u64,
        pub ri_resident_size: u64,
        pub ri_phys_footprint: u64,
        pub ri_proc_start_abstime: u64,
        pub ri_proc_exit_abstime: u64,
        pub ri_child_user_time: u64,
        pub ri_child_system_time: u64,
        pub ri_child_pkg_idle_wkups: u64,
        pub ri_child_interrupt_wkups: u64,
        pub ri_child_pageins: u64,
        pub ri_child_elapsed_abstime: u64,
        pub ri_diskio_bytesread: u64,
        pub ri_diskio_byteswritten: u64,
    }
    pub const RUSAGE_INFO_V2: i32 = 2;
    extern "C" {
        pub fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut RusageInfoV2) -> i32;
    }
}

/// Memory as Activity Monitor counts it (physical footprint) on macOS,
/// resident set size elsewhere. Bytes.
pub fn memory_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let mut info = mac::RusageInfoV2::default();
        // SAFETY: the kernel fills a struct of the requested flavor we own.
        let rc = unsafe { mac::proc_pid_rusage(std::process::id() as i32, mac::RUSAGE_INFO_V2, &mut info) };
        if rc == 0 {
            return info.ri_phys_footprint;
        }
        0
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .map(|kb| kb * 1024)
            .unwrap_or(0)
    }
}

/// Hands memory that malloc is holding but not using back to the OS. After
/// a large read (the first backfill parses hundreds of MB), macOS otherwise
/// keeps those freed pages charged to the process. Returns bytes released.
pub fn release_free_memory() -> usize {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut libc::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone means "all zones"; goal 0 means "as much as possible".
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) }
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

impl Sys {
    pub fn new() -> Self {
        Sys { last_cpu: cpu_seconds(), last_wall: Instant::now(), started: Instant::now(), cpu_pct: 0.0 }
    }

    /// Updates the CPU percentage at most every 5 seconds.
    pub fn sample(&mut self) {
        let wall = self.last_wall.elapsed().as_secs_f64();
        if wall < 5.0 {
            return;
        }
        let cpu = cpu_seconds();
        self.cpu_pct = ((cpu - self.last_cpu) / wall * 100.0).max(0.0);
        self.last_cpu = cpu;
        self.last_wall = Instant::now();
    }
}
