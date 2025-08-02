# Gorm - Mobile-optimized Linux WiFi settings

Gorm is a Wayland application that allows configuring your WiFi connections
through a touch friendly and efficient GUI.

## Permissions

The following polkit rules are required to allow users of the group `wheel` to
control all WiFi settings:

> /etc/polkit-1/rules.d/10-network-manager.rules

```
// Allow wheel users to request WiFi scans.
polkit.addRule(function(action, subject) {
	if (action.id == "org.freedesktop.NetworkManager.wifi.scan" && subject.isInGroup("wheel")) {
		return "yes";
	}
});

// Allow wheel users to disable WiFi networks.
polkit.addRule(function(action, subject) {
	if (action.id == "org.freedesktop.NetworkManager.network-control" && subject.isInGroup("wheel")) {
		return "yes";
	}
});

// Allow creating new network connections.
polkit.addRule(function(action, subject) {
	if (action.id == "org.freedesktop.NetworkManager.settings.modify.system" && subject.isInGroup("wheel")) {
		return "yes";
	}
});

// Allow wheel users to enable/disable WiFi.
polkit.addRule(function(action, subject) {
	if (action.id == "org.freedesktop.NetworkManager.enable-disable-wifi" && subject.isInGroup("wheel")) {
		return "yes";
	}
});
```
