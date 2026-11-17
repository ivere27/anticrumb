//! Cross-platform terminal environment guards and console codepage handling.

#[cfg(windows)]
mod sys {
    use windows_sys::Win32::System::Console::{
        GetConsoleCP, GetConsoleOutputCP, SetConsoleCP, SetConsoleOutputCP,
    };

    pub(crate) struct ConsoleGuard {
        orig_cp: u32,
        orig_output_cp: u32,
    }

    impl ConsoleGuard {
        pub(crate) fn init() -> Self {
            let (orig_cp, orig_output_cp) = unsafe {
                let cp = GetConsoleCP();
                let out_cp = GetConsoleOutputCP();
                // 65001 is UTF-8 codepage
                SetConsoleCP(65001);
                SetConsoleOutputCP(65001);
                (cp, out_cp)
            };
            Self {
                orig_cp,
                orig_output_cp,
            }
        }
    }

    impl Drop for ConsoleGuard {
        fn drop(&mut self) {
            unsafe {
                if self.orig_cp != 0 {
                    SetConsoleCP(self.orig_cp);
                }
                if self.orig_output_cp != 0 {
                    SetConsoleOutputCP(self.orig_output_cp);
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod sys {
    pub(crate) struct ConsoleGuard;

    impl ConsoleGuard {
        pub(crate) fn init() -> Self {
            Self
        }
    }
}

pub(crate) use sys::ConsoleGuard;
