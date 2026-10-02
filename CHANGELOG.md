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