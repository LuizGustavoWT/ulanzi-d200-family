use async_trait::async_trait;
use log::{info, warn};
use openaction::*;
use serde::{Deserialize, Serialize};

/// A configurable encoder action.
///
/// Place it on one of the D200X's three rotary encoders (or on a keypad cell)
/// to run shell commands when the dial is turned or pressed.
///
/// Placeholders available in the **turn** command:
///   %d  -> signed tick count (e.g. -2, +3). Negative = left, positive = right.
///   %D  -> unsigned magnitude with an explicit sign prefix (e.g. -2, +3),
///          useful for `wpctl set-volume @DEFAULT_AUDIO_SINK@ %D%`.
///   %a  -> absolute tick magnitude (always positive).
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct RotarySettings {
    /// Command run on every rotation. Supports %d / %D / %a placeholders.
    pub rotate: String,
    /// Command run on press (encoder push or key tap).
    pub press: String,
    /// Multiplier for ticks (1, 5, 10...)
    #[serde(default = "default_step")]
    pub step: u8,
}

fn default_step() -> u8 { 1 }

pub struct RotaryAction;

impl RotaryAction {
    /// Run a shell command on the host, escaping it for a single-quoted
    /// argument so the host shell sees it verbatim.
    async fn run_host(command: &str) {
        let escaped = command.replace('\'', "'\\''");
        match tokio::process::Command::new("flatpak-spawn")
            .args(["--host", "sh", "-c", &escaped])
            .status()
            .await
        {
            Ok(status) if !status.success() => {
                warn!("Rotary action (host) exited with {status}")
            }
            Ok(_) => {}
            Err(e) => warn!("flatpak-spawn could not start: {e}"),
        }
    }

    async fn run(command: &str) {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return;
        }
        info!("Rotary action: {trimmed}");

        // OpenDeck ships as a Flatpak, whose PATH has no wpctl/playerctl/
        // brightnessctl. Detect that and hop out to the host, where those
        // tools live and where the audio session actually is.
        const HOST_TOOLS: [&str; 5] = [
            "wpctl",
            "playerctl",
            "brightnessctl",
            "xdotool",
            "ydotool",
        ];
        let looks_like_flatpak = std::path::Path::new("/.flatpak-info").exists();
        if looks_like_flatpak && HOST_TOOLS.iter().any(|tool| trimmed.contains(tool)) {
            info!("Running on the host via flatpak-spawn: {trimmed}");
            Self::run_host(trimmed).await;
            return;
        }

        match tokio::process::Command::new("/bin/sh")
            .args(["-c", trimmed])
            .status()
            .await
        {
            Ok(status) if !status.success() => {
                warn!("Rotary action exited with {status}")
            }
            Ok(_) => {}
            Err(e) => warn!("Rotary action could not start: {e}"),
        }
    }

    /// Expand the turn-command placeholders for a given tick count.
    ///
    /// | token | becomes | example (ticks = -2)   | use
    /// |-------|---------|-------------------------|-----
    /// | `%d`  | signed number     | `-2`      | `playerctl seek %d`
    /// | `%D`  | number with `+`   | `+2`      | rarely useful on its own
    /// | `%a`  | absolute value    | `2`       | `xdotool key --repeat %a`
    /// | `%v`  | wpctl relative vol| `0.02-`   | `wpctl set-volume ... %v`
    /// | `%%`  | literal `%`       | `%`       | escape for a percent sign
    ///
    /// `%v` exists because `wpctl set-volume` has its own, unusual relative
    /// syntax: the `+`/`-` goes *after* the number and the value is a fraction,
    /// so `+5%` is rejected while `0.05+` is accepted.
    fn expand(template: &str, ticks: i16) -> String {
        let signed = format!("{:+}", ticks); // "-2" or "+2"
        let magnitude = ticks.unsigned_abs();
        let abs = magnitude.to_string();
        // wpctl wants a 0..1 fraction with a trailing + or -.
        let vol = format!(
            "{:.2}{}",
            f64::from(magnitude) / 100.0,
            if ticks < 0 { '-' } else { '+' }
        );
        let expanded = template
            .replace("%v", &vol)
            .replace("%D", &signed)
            .replace("%d", &ticks.to_string())
            .replace("%a", &abs);
        expanded.replace("%%", "%")
    }
}

#[async_trait]
impl Action for RotaryAction {
    const UUID: ActionUuid = "com.glmagalhaes.ulanzi.d200.rotary";
    type Settings = RotarySettings;

    async fn dial_rotate(
        &self,
        _instance: &Instance,
        settings: &Self::Settings,
        ticks: i16,
        _pressed: bool,
    ) -> OpenActionResult<()> {
        let step = if settings.step == 0 { 1 } else { settings.step as i16 };
        let scaled_ticks = ticks * step;
        let command = Self::expand(&settings.rotate, scaled_ticks);
        Self::run(&command).await;
        Ok(())
    }

    async fn dial_down(
        &self,
        _instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        Self::run(&settings.press).await;
        Ok(())
    }

    async fn key_down(
        &self,
        _instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        Self::run(&settings.press).await;
        Ok(())
    }

    async fn will_appear(&self, _: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_wpctl_relative_volume() {
        // `wpctl set-volume` rejects "+5%"; it wants "0.05+".
        assert_eq!(
            RotaryAction::expand("wpctl set-volume @DEFAULT_AUDIO_SINK@ %v", 5),
            "wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.05+"
        );
        assert_eq!(
            RotaryAction::expand("wpctl set-volume @DEFAULT_AUDIO_SINK@ %v", -5),
            "wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.05-"
        );
        assert_eq!(
            RotaryAction::expand("wpctl set-volume @DEFAULT_AUDIO_SINK@ %v", 1),
            "wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.01+"
        );
    }

    #[test]
    fn test_expand_plain_and_absolute() {
        assert_eq!(RotaryAction::expand("seek %d", -2), "seek -2");
        assert_eq!(RotaryAction::expand("step %a", -2), "step 2");
        assert_eq!(RotaryAction::expand("plus %D", 3), "plus +3");
    }

    #[test]
    fn test_expand_percent_escape() {
        assert_eq!(RotaryAction::expand("echo 100%%", 1), "echo 100%");
    }

    #[test]
    fn test_expand_without_placeholder_is_unchanged() {
        assert_eq!(RotaryAction::expand("playerctl next", 3), "playerctl next");
    }
}
