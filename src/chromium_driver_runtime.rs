use crate::chromium_driver::ChromiumDriver;
use std::sync::{Arc, OnceLock};

static DRIVER: OnceLock<Arc<ChromiumDriver>> = OnceLock::new();

pub fn install(driver: Arc<ChromiumDriver>) -> Result<(), Arc<ChromiumDriver>> {
    DRIVER.set(driver)
}

pub fn get() -> Option<&'static Arc<ChromiumDriver>> {
    DRIVER.get()
}
