use crate::common::{self, Server};
use anyhow::{Context, Result, ensure};
use clap::Args;
use serde_json::json;
use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

#[derive(Args)]
pub struct Options {
    pub binary: PathBuf,
    #[arg(long,default_value_t=20,value_parser=clap::value_parser!(u64).range(1..=3600))]
    pub seconds: u64,
    #[arg(long)]
    pub package: Option<PathBuf>,
}

#[cfg(windows)]
pub fn sample(pid: u32) -> Result<(u64, f64)> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ},
        },
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        ensure!(
            !handle.is_null(),
            "Cannot inspect process {pid}: {}",
            std::io::Error::last_os_error()
        );
        let result = (|| {
            let mut memory: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            memory.cb = std::mem::size_of_val(&memory) as u32;
            ensure!(
                GetProcessMemoryInfo(handle, &mut memory, std::mem::size_of_val(&memory) as u32)
                    != 0,
                "Cannot read process memory"
            );
            let mut created: FILETIME = std::mem::zeroed();
            let mut exited: FILETIME = std::mem::zeroed();
            let mut kernel: FILETIME = std::mem::zeroed();
            let mut user: FILETIME = std::mem::zeroed();
            ensure!(
                GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0,
                "Cannot read process CPU time"
            );
            let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
            Ok((
                memory.WorkingSetSize as u64,
                (ticks(kernel) + ticks(user)) as f64 / 10_000_000.0,
            ))
        })();
        CloseHandle(handle);
        result
    }
}
#[cfg(target_os = "linux")]
pub fn sample(pid: u32) -> Result<(u64, f64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields: Vec<_> = stat
        .rsplit_once(')')
        .context("Malformed proc stat")?
        .1
        .split_whitespace()
        .collect();
    ensure!(fields.len() > 21, "Truncated proc stat");
    let ticks = fields[11].parse::<u64>()? + fields[12].parse::<u64>()?;
    let rss = fields[21].parse::<u64>()?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let frequency = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    ensure!(
        page > 0 && frequency > 0,
        "Invalid system page size/clock frequency"
    );
    Ok((rss * page as u64, ticks as f64 / frequency as f64))
}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn sample(pid: u32) -> Result<(u64, f64)> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-o", "time=", "-p", &pid.to_string()])
        .output()?;
    ensure!(
        output.status.success(),
        "ps could not inspect process {pid}"
    );
    let text = String::from_utf8(output.stdout)?;
    let mut fields = text.split_whitespace();
    let memory = fields
        .next()
        .context("Missing resident memory")?
        .parse::<u64>()?
        * 1024;
    let time = fields.next().context("Missing CPU time")?;
    let (days, time) = if let Some((days, time)) = time.split_once('-') {
        (days.parse::<f64>()?, time)
    } else {
        (0.0, time)
    };
    let mut seconds = 0.0;
    for part in time.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>()?;
    }
    Ok((memory, days * 86400.0 + seconds))
}
pub fn run(options: Options) -> Result<()> {
    let binary = crate::absolute(&options.binary)?;
    let mut server = Server::new(&binary, "benchmark")?;
    let owner = server.initialize()?;
    if let Some(package) = &options.package {
        let manifest = common::package_manifest(package)?;
        let id = manifest["id"].as_str().context("Package has no app ID")?;
        ensure!(
            manifest["runtime"] == "wasi",
            "Benchmarks accept WASI apps only"
        );
        server.install(&owner, package, id)?;
    }
    thread::sleep(Duration::from_secs(3));
    let pid = server.pid()?;
    let (before_memory, before_cpu) = sample(pid)?;
    println!(
        "Sampling initialized idle server for {} seconds...",
        options.seconds
    );
    let start = Instant::now();
    thread::sleep(Duration::from_secs(options.seconds));
    let (memory, cpu) = sample(pid)?;
    let elapsed = start.elapsed().as_secs_f64();
    let value = json!({"platform":format!("{}-{}",std::env::consts::OS,std::env::consts::ARCH),"processor":std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_|std::env::consts::ARCH.into()),"binary":binary,"initialized":true,"sample_seconds":elapsed,"resident_mib":before_memory.max(memory) as f64/1048576.0,"cpu_percent_one_core":(cpu-before_cpu)/elapsed*100.0,"apps":usize::from(options.package.is_some()),"runtime":options.package.as_ref().map(|_|"wasi"),"caddy":false});
    common::report(
        if options.package.is_some() {
            "benchmark-wasi-last.json"
        } else {
            "benchmark-last.json"
        },
        &value,
    )
}
