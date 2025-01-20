use serde::Serialize;

#[derive(clap::ValueEnum, Clone, Default, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceProfile {
    #[default]
    KindlePaperwhite2024, // todo
    Remarkable2,
}

impl DeviceProfile {
    pub fn max_width(&self) -> usize {
        match self {
            DeviceProfile::KindlePaperwhite2024 => 1264,
            DeviceProfile::Remarkable2 => 1404,
        }
    }

    pub fn is_grayscale(&self) -> bool {
        match self {
            DeviceProfile::KindlePaperwhite2024 => true,
            DeviceProfile::Remarkable2 => true,
        }
    }
}
