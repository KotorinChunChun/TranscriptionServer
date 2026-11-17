use anyhow::{Result, bail};
use std::{path::Path, process::Stdio};
pub fn command(program: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    cmd
}
pub fn browser(url: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}
pub fn set_title() {
    #[cfg(windows)]
    {
        let title = format!(
            "サーバー - TranscriptionServer - v{}\0",
            crate::config::VERSION
        )
        .encode_utf16()
        .collect::<Vec<_>>();
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleTitleW(title.as_ptr());
        }
    }
}
#[cfg(windows)]
mod win {
    use super::*;
    use windows_sys::Win32::{
        Foundation::*,
        System::{JobObjects::*, Threading::*},
    };
    pub struct Job(usize);
    impl Job {
        pub fn new() -> Result<Self> {
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return Err(std::io::Error::last_os_error().into());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    (&info as *const _) as _,
                    std::mem::size_of_val(&info) as u32,
                ) == 0
                {
                    CloseHandle(h);
                    return Err(std::io::Error::last_os_error().into());
                }
                Ok(Self(h as usize))
            }
        }
        pub fn assign(&self, child: &tokio::process::Child) -> Result<()> {
            let h = child
                .raw_handle()
                .ok_or_else(|| anyhow::anyhow!("子プロセスを取得できません"))?;
            unsafe {
                if AssignProcessToJobObject(self.0 as _, h as _) == 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            Ok(())
        }
        /// Sum PrivateUsage of this owned Job Object's live process tree. Not working set or VRAM.
        pub fn memory_bytes(&self) -> Option<u64> {
            use windows_sys::Win32::System::ProcessStatus::*;
            unsafe {
                for capacity in [64usize, 256, 1024, 4096] {
                    let mut buffer = vec![0usize; capacity + 2];
                    if QueryInformationJobObject(
                        self.0 as _,
                        JobObjectBasicProcessIdList,
                        buffer.as_mut_ptr() as _,
                        (8 + capacity * std::mem::size_of::<usize>()) as u32,
                        std::ptr::null_mut(),
                    ) == 0
                    {
                        if GetLastError() == ERROR_MORE_DATA {
                            continue;
                        }
                        return None;
                    }
                    let count = *((buffer.as_ptr() as *const u32).add(1)) as usize;
                    if count == 0 || count > capacity {
                        return None;
                    }
                    let ids = (buffer.as_ptr() as *const u8).add(8) as *const usize;
                    let mut total = 0u64;
                    for index in 0..count {
                        let process = OpenProcess(
                            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
                            0,
                            *ids.add(index) as u32,
                        );
                        if process.is_null() {
                            return None;
                        }
                        let mut info: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
                        info.cb = std::mem::size_of_val(&info) as u32;
                        let ok = K32GetProcessMemoryInfo(
                            process,
                            (&mut info as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
                            info.cb,
                        );
                        CloseHandle(process);
                        if ok == 0 {
                            return None;
                        }
                        total = total.checked_add(info.PrivateUsage as u64)?;
                    }
                    return Some(total);
                }
                None
            }
        }
        pub fn terminate(&self) -> Result<()> {
            unsafe {
                if TerminateJobObject(self.0 as _, 1) == 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            Ok(())
        }
        pub fn empty(&self) -> Result<bool> {
            unsafe {
                let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
                if QueryInformationJobObject(
                    self.0 as _,
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut _) as _,
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                ) == 0
                {
                    return Err(std::io::Error::last_os_error().into());
                };
                Ok(info.ActiveProcesses == 0)
            }
        }
        pub fn detach(&self) -> Result<()> {
            unsafe {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                // Future update helpers must not accumulate inherited job
                // hierarchies over many successive server updates.
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
                if SetInformationJobObject(
                    self.0 as _,
                    JobObjectExtendedLimitInformation,
                    (&info as *const _) as _,
                    std::mem::size_of_val(&info) as u32,
                ) == 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            Ok(())
        }
    }
    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0 as _);
            }
        }
    }
    pub struct Instance {
        handle: usize,
        pub existing: bool,
    }
    impl Instance {
        pub fn new() -> Result<Self> {
            let name = "Local\\TranscriptionServer.Installed\0"
                .encode_utf16()
                .collect::<Vec<_>>();
            unsafe {
                let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
                if handle.is_null() {
                    return Err(std::io::Error::last_os_error().into());
                }
                Ok(Self {
                    handle: handle as usize,
                    existing: GetLastError() == ERROR_ALREADY_EXISTS,
                })
            }
        }
    }
    impl Drop for Instance {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle as _);
            }
        }
    }
    pub fn identity(pid: u32) -> Result<Option<u64>> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | 0x100000, 0, pid);
            if h.is_null() {
                if GetLastError() == ERROR_INVALID_PARAMETER {
                    return Ok(None);
                }
                return Err(std::io::Error::last_os_error().into());
            };
            let state = WaitForSingleObject(h, 0);
            if state == WAIT_OBJECT_0 {
                CloseHandle(h);
                return Ok(None);
            }
            if state != WAIT_TIMEOUT {
                CloseHandle(h);
                bail!("プロセス状態を確認できません")
            };
            let (mut a, mut b, mut c, mut d) = (
                std::mem::zeroed(),
                std::mem::zeroed(),
                std::mem::zeroed(),
                std::mem::zeroed(),
            );
            let ok = GetProcessTimes(h, &mut a, &mut b, &mut c, &mut d);
            CloseHandle(h);
            if ok == 0 {
                bail!("プロセス開始時刻を確認できません")
            };
            Ok(Some(
                ((a.dwHighDateTime as u64) << 32) | a.dwLowDateTime as u64,
            ))
        }
    }
    pub fn image_path(pid: u32) -> Result<std::path::PathBuf> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut buffer = vec![0u16; 32768];
            let mut length = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length);
            CloseHandle(handle);
            if ok == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(std::path::PathBuf::from(String::from_utf16(
                &buffer[..length as usize],
            )?))
        }
    }
    pub fn restore_display_version(root: &Path, version: &str) -> Result<()> {
        use windows_sys::Win32::System::Registry::*;
        let key = format!(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{}_is1\0",
            crate::config::APP_ID
        )
        .encode_utf16()
        .collect::<Vec<_>>();
        let location = "InstallLocation\0".encode_utf16().collect::<Vec<_>>();
        unsafe {
            let mut size = 0;
            let code = RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                location.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            );
            if code == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            if code != ERROR_SUCCESS || size > 32768 {
                return Err(std::io::Error::from_raw_os_error(code as i32).into());
            };
            let mut bytes = vec![0u16; (size as usize).div_ceil(2)];
            if RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                location.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                bytes.as_mut_ptr() as _,
                &mut size,
            ) != ERROR_SUCCESS
            {
                bail!("導入先の照合に失敗しました")
            };
            let location = String::from_utf16_lossy(&bytes)
                .trim_end_matches('\0')
                .to_string();
            if std::fs::canonicalize(location)? != std::fs::canonicalize(root)? {
                bail!("登録された導入先が異なるためレジストリを変更しません")
            };
            let mut handle = std::ptr::null_mut();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                0,
                KEY_SET_VALUE,
                &mut handle,
            ) != ERROR_SUCCESS
            {
                bail!("版登録を復旧できません")
            };
            let name = "DisplayVersion\0".encode_utf16().collect::<Vec<_>>();
            let value = format!("{version}\0").encode_utf16().collect::<Vec<_>>();
            let code = RegSetValueExW(
                handle,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr() as _,
                (value.len() * 2) as u32,
            );
            RegCloseKey(handle);
            if code != ERROR_SUCCESS {
                bail!("版登録を復旧できません")
            }
        }
        Ok(())
    }
}
#[cfg(windows)]
pub use win::*;
#[cfg(not(windows))]
pub struct Job;
#[cfg(not(windows))]
impl Job {
    pub fn memory_bytes(&self) -> Option<u64> {
        None
    }
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
    pub fn assign(&self, _: &tokio::process::Child) -> Result<()> {
        Ok(())
    }
    pub fn terminate(&self) -> Result<()> {
        Ok(())
    }
    pub fn empty(&self) -> Result<bool> {
        Ok(true)
    }
    pub fn detach(&self) -> Result<()> {
        Ok(())
    }
}
#[cfg(not(windows))]
pub fn restore_display_version(_: &Path, _: &str) -> Result<()> {
    Ok(())
}
#[cfg(not(windows))]
pub fn identity(pid: u32) -> Result<Option<u64>> {
    Ok(Path::new(&format!("/proc/{pid}"))
        .exists()
        .then_some(pid as u64))
}
#[cfg(not(windows))]
pub struct Instance {
    pub existing: bool,
}
#[cfg(not(windows))]
pub fn image_path(pid: u32) -> Result<std::path::PathBuf> {
    Ok(std::fs::read_link(format!("/proc/{pid}/exe"))?)
}
#[cfg(not(windows))]
impl Instance {
    pub fn new() -> Result<Self> {
        Ok(Self { existing: false })
    }
}
pub fn process_record(path: &Path, port: u16, instance_id: &str) -> Result<()> {
    let created = identity(std::process::id())?
        .ok_or_else(|| anyhow::anyhow!("実行プロセスを確認できません"))?;
    #[cfg(windows)]
    let stamp = {
        let unix = created as i64 - 116444736000000000;
        let date =
            chrono::DateTime::from_timestamp(unix / 10000000, ((unix % 10000000) * 100) as u32)
                .ok_or_else(|| anyhow::anyhow!("開始時刻が不正です"))?;
        format!(
            "{}.{:07}Z",
            date.format("%Y-%m-%dT%H:%M:%S"),
            unix % 10000000
        )
    };
    #[cfg(not(windows))]
    let stamp = {
        let _ = created;
        crate::store::now()
    };
    crate::config::write_json(
        path,
        &serde_json::json!({"pid":std::process::id(),"executable":crate::config::friendly(&std::env::current_exe()?),"started_at":stamp,"port":port,"instanceId":instance_id}),
    )
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};
    #[tokio::test]
    async fn job_drop_stops_owned_descendants_and_closed_process_is_not_alive() {
        let scope = Job::new().unwrap();
        let script = "$child=Start-Process pwsh -WindowStyle Hidden -ArgumentList '-NoProfile -NonInteractive -Command Start-Sleep -Seconds 30' -PassThru; Write-Output $child.Id; Start-Sleep -Seconds 30";
        let mut child = command(Path::new("pwsh"))
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(false)
            .spawn()
            .unwrap();
        scope.assign(&child).unwrap();
        let parent = child.id().unwrap();
        assert!(identity(parent).unwrap().is_some());
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            reader.read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let grandchild: u32 = line.trim().parse().unwrap();
        assert!(identity(grandchild).unwrap().is_some());
        drop(scope);
        tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(identity(parent).unwrap().is_none());
        for _ in 0..100 {
            if identity(grandchild).unwrap().is_none() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("owned descendant survived its job object");
    }
}
