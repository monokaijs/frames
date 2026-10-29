/// A per-parent job ensures an abruptly closed host does not orphan codec workers.
#[cfg(windows)]
pub fn track(child: &tokio::process::Child) -> anyhow::Result<()> {
    use windows::Win32::{Foundation::HANDLE, System::JobObjects::*};
    struct Job(HANDLE);
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}
    static JOB: std::sync::OnceLock<Result<Job, String>> = std::sync::OnceLock::new();
    let job = JOB
        .get_or_init(|| unsafe {
            let create = (|| -> anyhow::Result<Job> {
                let handle = CreateJobObjectW(None, None)?;
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of_val(&info) as u32,
                )?;
                Ok(Job(handle))
            })();
            create.map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    if let Some(handle) = child.raw_handle() {
        unsafe {
            AssignProcessToJobObject(job.0, HANDLE(handle))?;
        }
    }
    Ok(())
}
#[cfg(not(windows))]
pub fn track(_: &tokio::process::Child) -> anyhow::Result<()> {
    Ok(())
}
