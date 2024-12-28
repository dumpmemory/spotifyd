use crate::utils;
use clap::{
    builder::{IntoResettable, PossibleValuesParser, TypedValueParser, ValueParser},
    Args, Parser, ValueEnum,
};
use color_eyre::Report;
use directories::ProjectDirs;
use gethostname::gethostname;
use librespot_core::{cache::Cache, config::DeviceType as LSDeviceType, config::SessionConfig};
use librespot_playback::{
    audio_backend,
    config::{AudioFormat as LSAudioFormat, Bitrate as LSBitrate, PlayerConfig},
    dither::{mk_ditherer, DithererBuilder, TriangularDitherer},
};
use log::{error, info, warn};
use serde::{de::Error, de::Unexpected, Deserialize, Deserializer};
use sha1::{Digest, Sha1};
use std::{fs, path::Path, path::PathBuf};
use url::Url;

const CONFIG_FILE_NAME: &str = "spotifyd.conf";

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum VolumeController {
    #[cfg(feature = "alsa_backend")]
    Alsa,
    #[cfg(feature = "alsa_backend")]
    AlsaLinear,
    #[serde(rename = "softvol")]
    SoftVolume,
    None,
}

// Spotify's device type (copied from it's config.rs)
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum DeviceType {
    Unknown,
    Computer,
    Tablet,
    Smartphone,
    Speaker,
    #[serde(rename = "t_v")]
    Tv,
    #[serde(rename = "a_v_r")]
    Avr,
    #[serde(rename = "s_t_b")]
    Stb,
    AudioDongle,
    GameConsole,
    CastAudio,
    CastVideo,
    Automobile,
    Smartwatch,
    Chromebook,
    UnknownSpotify,
    CarThing,
    Observer,
    HomeThing,
}

impl From<DeviceType> for LSDeviceType {
    fn from(item: DeviceType) -> Self {
        match item {
            DeviceType::Unknown => LSDeviceType::Unknown,
            DeviceType::Computer => LSDeviceType::Computer,
            DeviceType::Tablet => LSDeviceType::Tablet,
            DeviceType::Smartphone => LSDeviceType::Smartphone,
            DeviceType::Speaker => LSDeviceType::Speaker,
            DeviceType::Tv => LSDeviceType::Tv,
            DeviceType::Avr => LSDeviceType::Avr,
            DeviceType::Stb => LSDeviceType::Stb,
            DeviceType::AudioDongle => LSDeviceType::AudioDongle,
            DeviceType::GameConsole => LSDeviceType::GameConsole,
            DeviceType::CastAudio => LSDeviceType::CastAudio,
            DeviceType::CastVideo => LSDeviceType::CastVideo,
            DeviceType::Automobile => LSDeviceType::Automobile,
            DeviceType::Smartwatch => LSDeviceType::Smartwatch,
            DeviceType::Chromebook => LSDeviceType::Chromebook,
            DeviceType::UnknownSpotify => LSDeviceType::UnknownSpotify,
            DeviceType::CarThing => LSDeviceType::CarThing,
            DeviceType::Observer => LSDeviceType::Observer,
            DeviceType::HomeThing => LSDeviceType::HomeThing,
        }
    }
}

fn bitrate_parser() -> impl IntoResettable<ValueParser> {
    let possible_values: PossibleValuesParser = ["96", "160", "320"].into();
    possible_values.map(|val| match val.as_str() {
        "96" => Bitrate::Bitrate96,
        "160" => Bitrate::Bitrate160,
        "320" => Bitrate::Bitrate320,
        _ => unreachable!(),
    })
}

/// Spotify's audio bitrate
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Bitrate {
    Bitrate96,
    Bitrate160,
    Bitrate320,
}

impl<'de> Deserialize<'de> for Bitrate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match u16::deserialize(deserializer) {
            Ok(96) => Ok(Self::Bitrate96),
            Ok(160) => Ok(Self::Bitrate160),
            Ok(320) => Ok(Self::Bitrate320),
            Ok(x) => Err(D::Error::invalid_value(
                Unexpected::Unsigned(x.into()),
                &"a bitrate: 96, 160, 320",
            )),
            Err(e) => Err(e),
        }
    }
}

impl From<Bitrate> for LSBitrate {
    fn from(bitrate: Bitrate) -> Self {
        match bitrate {
            Bitrate::Bitrate96 => LSBitrate::Bitrate96,
            Bitrate::Bitrate160 => LSBitrate::Bitrate160,
            Bitrate::Bitrate320 => LSBitrate::Bitrate320,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum DBusType {
    Session,
    System,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, ValueEnum)]
pub enum AudioFormat {
    F32,
    S32,
    S24,
    S24_3,
    S16,
}

impl From<AudioFormat> for LSAudioFormat {
    fn from(audio_format: AudioFormat) -> Self {
        match audio_format {
            AudioFormat::F32 => LSAudioFormat::F32,
            AudioFormat::S32 => LSAudioFormat::S32,
            AudioFormat::S24 => LSAudioFormat::S24,
            AudioFormat::S24_3 => LSAudioFormat::S24_3,
            AudioFormat::S16 => LSAudioFormat::S16,
        }
    }
}

fn possible_backends() -> Vec<&'static str> {
    audio_backend::BACKENDS.iter().map(|b| b.0).collect()
}

#[derive(Debug, Default, Parser)]
#[command(version, about, long_about = None)]
pub struct CliConfig {
    /// The path to the config file to use
    #[arg(long, value_name = "PATH")]
    pub config_path: Option<PathBuf>,

    /// If set, starts spotifyd without detaching
    #[arg(long)]
    pub no_daemon: bool,

    /// Prints more verbose output
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Path to PID file.
    #[cfg(unix)]
    #[arg(long, value_name = "PATH")]
    pub pid: Option<PathBuf>,

    #[command(flatten)]
    pub shared_config: SharedConfigValues,
}

// A struct that holds all allowed config fields.
// The actual config file is made up of two sections, spotifyd and global.
#[derive(Clone, Default, Debug, Deserialize, PartialEq, Args)]
pub struct SharedConfigValues {
    /// A script that gets evaluated in the user's shell when the song changes
    #[arg(visible_alias = "onevent", long, value_name = "CMD")]
    #[serde(alias = "onevent")]
    on_song_change_hook: Option<String>,

    /// The cache path used to store credentials and music file artifacts
    #[arg(long, short, value_name = "PATH")]
    cache_path: Option<PathBuf>,

    /// The maximal cache size in bytes
    #[arg(long, value_name = "BYTES")]
    max_cache_size: Option<u64>,

    /// Disable the use of audio cache
    #[arg(
        long,
        default_missing_value("true"),
        require_equals = true,
        num_args(0..=1),
        value_name = "BOOL"
    )]
    no_audio_cache: Option<bool>,

    /// The audio backend to use
    #[arg(long, short, value_parser = possible_backends())]
    backend: Option<String>,

    /// The volume controller to use
    #[arg(value_enum, long, visible_alias = "volume-control")]
    #[serde(alias = "volume-control")]
    volume_controller: Option<VolumeController>,

    /// The audio device (or pipe file)
    #[arg(long)]
    device: Option<String>,

    /// The device name displayed in Spotify
    #[arg(long, short)]
    device_name: Option<String>,

    /// The bitrate of the streamed audio data
    #[arg(long, short = 'B', value_parser = bitrate_parser())]
    bitrate: Option<Bitrate>,

    /// The audio format of the streamed audio data
    #[arg(value_enum, long)]
    audio_format: Option<AudioFormat>,

    /// Initial volume between 0 and 100
    #[arg(long)]
    initial_volume: Option<u8>,

    /// Enable to normalize the volume during playback
    #[arg(
        long,
        default_missing_value("true"),
        require_equals = true,
        num_args(0..=1),
        value_name = "BOOL"
    )]
    volume_normalisation: Option<bool>,

    /// A custom pregain applied before sending the audio to the output device
    #[arg(long)]
    normalisation_pregain: Option<f64>,

    #[arg(
        long,
        default_missing_value("true"),
        require_equals = true,
        num_args(0..=1),
        value_name = "BOOL"
    )]
    disable_discovery: Option<bool>,

    /// The port used for the Spotify Connect discovery
    #[arg(long)]
    zeroconf_port: Option<u16>,

    /// The proxy used to connect to spotify's servers
    #[arg(long, value_name = "URL")]
    proxy: Option<String>,

    /// The device type shown to clients
    #[arg(value_enum, long)]
    device_type: Option<DeviceType>,

    /// Start playing similar songs after your music has ended
    #[arg(
        long,
        default_missing_value("true"),
        require_equals = true,
        num_args(0..=1),
        value_name = "BOOL"
    )]
    #[serde(default)]
    autoplay: Option<bool>,

    #[cfg(feature = "alsa_backend")]
    #[command(flatten)]
    #[serde(flatten)]
    alsa_config: AlsaConfig,

    #[cfg(feature = "dbus_mpris")]
    #[command(flatten)]
    #[serde(flatten)]
    mpris_config: MprisConfig,
}

#[cfg(feature = "dbus_mpris")]
#[derive(Debug, Default, Clone, Deserialize, Args, PartialEq, Eq)]
pub struct MprisConfig {
    /// Enables the MPRIS interface
    #[arg(
        long,
        default_missing_value("true"),
        require_equals = true,
        num_args(0..=1),
        value_name = "BOOL"
    )]
    #[serde(alias = "use-mpris")]
    pub(crate) use_mpris: Option<bool>,

    /// The Bus-type to use for the MPRIS interface
    #[arg(value_enum, long)]
    pub(crate) dbus_type: Option<DBusType>,
}

#[cfg(feature = "alsa_backend")]
#[derive(Debug, Default, Clone, Deserialize, Args, PartialEq, Eq)]
pub struct AlsaConfig {
    /// The control device
    #[arg(long)]
    pub(crate) control: Option<String>,

    /// The mixer to use
    #[arg(long)]
    pub(crate) mixer: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct FileConfig {
    global: Option<SharedConfigValues>,
    spotifyd: Option<SharedConfigValues>,
}

impl FileConfig {
    pub fn get_merged_sections(self) -> Option<SharedConfigValues> {
        match (self.global, self.spotifyd) {
            (Some(global), Some(mut spotifyd)) => {
                spotifyd.merge_with(global);
                Some(spotifyd)
            }
            (global, spotifyd) => global.or(spotifyd),
        }
    }
}

impl CliConfig {
    pub fn load_config_file_values(&mut self) -> Result<(), Report> {
        let config_file_path = match self.config_path.clone().or_else(get_config_file) {
            Some(p) => p,
            None => {
                info!("No config file specified. Running with default values");
                return Ok(());
            }
        };
        info!("Loading config from {:?}", &config_file_path);

        let content = match fs::read_to_string(config_file_path) {
            Ok(s) => s,
            Err(e) => {
                info!("Failed reading config file: {}", e);
                return Ok(());
            }
        };

        let config_content: FileConfig = toml::from_str(&content)?;

        // The call to get_merged_sections consumes the FileConfig!
        if let Some(merged_sections) = config_content.get_merged_sections() {
            self.shared_config.merge_with(merged_sections);
        }

        Ok(())
    }
}

impl SharedConfigValues {
    pub fn merge_with(&mut self, mut other: SharedConfigValues) {
        macro_rules! merge {
            ($a:expr; and $b:expr => {$($x:ident),+}) => {
                $($a.$x = $a.$x.take().or_else(|| $b.$x.take());)+
            }
        }

        // Handles Option<T> merging.
        merge!(self; and other => {
            backend,
            volume_normalisation,
            normalisation_pregain,
            bitrate,
            initial_volume,
            device_name,
            device,
            volume_controller,
            cache_path,
            no_audio_cache,
            on_song_change_hook,
            disable_discovery,
            zeroconf_port,
            proxy,
            device_type,
            max_cache_size,
            audio_format,
            autoplay
        });

        #[cfg(feature = "dbus_mpris")]
        merge!(self.mpris_config; and other.mpris_config => {use_mpris, dbus_type});
        #[cfg(feature = "alsa_backend")]
        merge!(self.alsa_config; and other.alsa_config => {mixer, control});
    }
}

pub(crate) fn get_config_file() -> Option<PathBuf> {
    let etc_conf = format!("/etc/{}", CONFIG_FILE_NAME);
    let dirs = directories::ProjectDirs::from("", "", "spotifyd")?;
    let mut path = dirs.config_dir().to_path_buf();
    path.push(CONFIG_FILE_NAME);

    if path.exists() {
        Some(path)
    } else if Path::new(&etc_conf).exists() {
        let path: PathBuf = etc_conf.into();
        Some(path)
    } else {
        None
    }
}

fn device_id(name: &str) -> String {
    hex::encode(Sha1::digest(name.as_bytes()))
}

pub(crate) struct SpotifydConfig {
    pub(crate) cache: Option<Cache>,
    pub(crate) backend: Option<String>,
    pub(crate) audio_device: Option<String>,
    pub(crate) audio_format: LSAudioFormat,
    pub(crate) volume_controller: VolumeController,
    pub(crate) initial_volume: Option<u16>,
    pub(crate) device_name: String,
    pub(crate) player_config: PlayerConfig,
    pub(crate) session_config: SessionConfig,
    pub(crate) onevent: Option<String>,
    #[cfg(unix)]
    pub(crate) pid: Option<String>,
    pub(crate) shell: String,
    pub(crate) discovery: bool,
    pub(crate) zeroconf_port: Option<u16>,
    pub(crate) device_type: LSDeviceType,
    #[cfg(feature = "dbus_mpris")]
    pub(crate) mpris: MprisConfig,
    #[cfg(feature = "alsa_backend")]
    pub(crate) alsa_config: AlsaConfig,
}

pub(crate) fn get_internal_config(config: CliConfig) -> SpotifydConfig {
    let audio_cache = !config.shared_config.no_audio_cache.unwrap_or(false);

    let size_limit = config.shared_config.max_cache_size;
    let cache = config
        .shared_config
        .cache_path
        .or_else(|| {
            ProjectDirs::from("", "", "spotifyd").map(|dirs| dirs.cache_dir().to_path_buf())
        })
        .or_else(|| {
            warn!("failed to determine cache directory, please specify one manually!");
            None
        })
        .map(|path| {
            Cache::new(
                Some(&path),
                Some(&path),
                audio_cache.then_some(&path),
                size_limit,
            )
        })
        .transpose()
        .unwrap_or_else(|e| {
            warn!("Cache couldn't be initialized: {e}");
            None
        });

    let bitrate: LSBitrate = config
        .shared_config
        .bitrate
        .unwrap_or(Bitrate::Bitrate160)
        .into();

    let audio_format: LSAudioFormat = config
        .shared_config
        .audio_format
        .unwrap_or(AudioFormat::S16)
        .into();

    let volume_controller = config
        .shared_config
        .volume_controller
        .unwrap_or(VolumeController::SoftVolume);

    let initial_volume: Option<u16> = config
        .shared_config
        .initial_volume
        .filter(|val| {
            if (0..=100).contains(val) {
                true
            } else {
                warn!("initial_volume must be in range 0..100");
                false
            }
        })
        .map(|volume| (volume as i32 * (u16::MAX as i32) / 100) as u16);

    let device_name = config
        .shared_config
        .device_name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{}@{}", "Spotifyd", gethostname().to_string_lossy()));

    let device_id = device_id(&device_name);

    let normalisation_pregain = config.shared_config.normalisation_pregain.unwrap_or(0.0);

    let device_type = config
        .shared_config
        .device_type
        .unwrap_or(DeviceType::Speaker)
        .into();

    #[cfg(unix)]
    let pid = config.pid.map(|f| {
        f.into_os_string()
            .into_string()
            .expect("Failed to convert PID file path to valid Unicode")
    });

    let shell = utils::get_shell().unwrap_or_else(|| {
        info!("Unable to identify shell. Defaulting to \"sh\".");
        "sh".to_string()
    });

    let mut proxy_url = None;
    match config.shared_config.proxy {
        Some(s) => match Url::parse(&s) {
            Ok(url) => {
                if url.scheme() != "http" {
                    error!("Only HTTP proxies are supported!");
                } else {
                    proxy_url = Some(url);
                }
            }
            Err(err) => error!("Invalid proxy URL: {}", err),
        },
        None => info!("No proxy specified"),
    }

    // choose default ditherer the same way librespot does
    let ditherer: Option<DithererBuilder> = match audio_format {
        LSAudioFormat::S16 | LSAudioFormat::S24 | LSAudioFormat::S24_3 => {
            Some(mk_ditherer::<TriangularDitherer>)
        }
        _ => None,
    };

    // TODO: when we were on librespot 0.1.5, all PlayerConfig values were available in the
    //  Spotifyd config. The upgrade to librespot 0.2.0 introduces new config variables, and we
    //  should consider adding them to Spotifyd's config system.
    let pc = PlayerConfig {
        bitrate,
        normalisation: config.shared_config.volume_normalisation.unwrap_or(false),
        normalisation_pregain_db: normalisation_pregain,
        gapless: true,
        ditherer,
        ..Default::default()
    };

    SpotifydConfig {
        cache,
        backend: config.shared_config.backend,
        audio_device: config.shared_config.device,
        audio_format,
        volume_controller,
        initial_volume,
        device_name,
        player_config: pc,
        session_config: SessionConfig {
            autoplay: config.shared_config.autoplay,
            device_id,
            proxy: proxy_url,
            ap_port: Some(443),
            ..Default::default()
        },
        onevent: config.shared_config.on_song_change_hook,
        shell,
        discovery: !config.shared_config.disable_discovery.unwrap_or(false),
        zeroconf_port: config.shared_config.zeroconf_port,
        device_type,
        #[cfg(unix)]
        pid,
        #[cfg(feature = "dbus_mpris")]
        mpris: config.shared_config.mpris_config,
        #[cfg(feature = "alsa_backend")]
        alsa_config: config.shared_config.alsa_config,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_section_merging() {
        let mut spotifyd_section = SharedConfigValues {
            device_type: Some(DeviceType::Computer),
            ..Default::default()
        };

        let global_section = SharedConfigValues {
            device_name: Some("spotifyd-test".to_string()),
            ..Default::default()
        };

        // The test only makes sense if both sections differ.
        assert_ne!(spotifyd_section, global_section);

        let file_config = FileConfig {
            global: Some(global_section),
            spotifyd: Some(spotifyd_section.clone()),
        };
        let merged_config = file_config.get_merged_sections().unwrap();

        // Add the new field to spotifyd section.
        spotifyd_section.device_name = Some("spotifyd-test".to_string());
        assert_eq!(merged_config, spotifyd_section);
    }
}
