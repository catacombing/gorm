# Gorm - Mobile-optimized Linux WiFi settings

Gorm is a Wayland application that allows configuring your WiFi connections
through a touch friendly and efficient GUI.

## Permissions

To allow managing NetworkManager through DBus, Gorm requires some polkit
permissions. The rules to grant these permissions to users in the `catacomb`
group can be found at [./10-gorm.rules].
