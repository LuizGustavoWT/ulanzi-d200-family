# 0.8.0
* New "Rotary" action with a dropdown-based Property Inspector: pick a preset (volume, media, brightness, scroll) instead of typing a shell command
* Configurable Step multiplier so one detent can move 1, 5, 10 percent or more
* Commands run on the host via flatpak-spawn, since the OpenDeck Flatpak ships no wpctl/playerctl/brightnessctl
* Placeholders: %v (wpctl relative volume), %d (signed ticks), %a (absolute), %% (literal percent)
* Fixed encoder reports: byte 10 flags encoder traffic, byte 11 carries the dial event (0 release, 1 press, 2 left, 3 right)
* Fixed encoder images being painted onto keypad keys 1/2/3; non-Keypad controllers are now ignored
* Side buttons 15/16 no longer raise "button index out of range" when OpenDeck paints their icon
* ULANZI_INVERT_DIAL=1 flips encoder direction for units wired the other way round
* pack-local.sh builds and installs straight into the OpenDeck plugin folder, no git or release needed

# 0.7.3
* Register device with touchpoints: 2 so the D200X side buttons appear in the OpenDeck UI alongside the 3 encoders

# 0.7.2
* Correct D200X input mapping: 3 rotary encoders (volume) with press+rotate, 2 side buttons as touchpoints (15/16), grid layout 5+5+3+wide
* Unit tests covering encoder rotate/press, side buttons, phantom key and normal keys

# 0.7.1
* Auto-reconnect: the plugin now rescans for the device every 2 seconds, so plugging it in later (or granting udev access while it is already plugged) works without restarting OpenDeck
* Device disconnect is now reported to OpenDeck (deregisterDevice)
* Hardened udev rule + install-udev-rules.sh helper that applies permissions without unplugging


# 0.7.0
* Unified plugin: D200, D200H and D200X are now managed by a single install
* Image bundle covers the full 5x3 grid (15 slots), which is the full key count on the D200X
* Updated manifest/readme naming to reflect the whole D200 family


# 0.6.5
* Saving status window state from previous sessions #14
* Remove remaining code from stand alone daemon
* Cleaning config.yaml
* Stability/Security updates on rust and libraries

# 0.6.4
* Change reverse domain to com.glmagalhaes.ulanzi.d200, added the supported device that was missing
* This name will be final for automatic updates in the future
* Launched on OpenDeck's OpenAction Marketplace #10

# 0.6.3
* Support for GPU load in status window #8
* Better organization in code
* Change in plugin namming, internal and external #11

# 0.6.2
 * Mapped all the possible screens that the status window has
 * Added an action to witch cycle what's shown on status window

# 0.6.1
 * Improved algorithm circumventing the hardware bug adding a lot more entropy
 * Removed blinking caused by the device recieving too many packets too fast
 * Reduced the number of packages sent to the device

# 0.6.0
 * Improved on how to circumvent the hardware bug
 * Added an icon to the packaged version of the plug-in
 * Updated info to show that It also works with Ulanzi D200(H)
 * Added a shell script to package the plug-in correctly

# 0.5.0
 * Circumvented a known bug in the hardware crash depending on the values in certain zip positions
 * Reduced racing conditions when sending data to device