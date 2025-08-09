# Gorm - Mobile-optimized Linux WiFi settings

Gorm is a Wayland application that allows configuring your WiFi connections
through a touch friendly and efficient GUI.

## Screenshots

<p align="center">
  <img src="https://github.com/user-attachments/assets/48e4bbe1-4ed6-4967-bde5-c97c60b9f2ae" width="30%"/>
  <img src="https://github.com/user-attachments/assets/9f63d532-54a4-41f7-aa19-9bb0672ed2fd" width="30%"/>
</p>

## Permissions

To allow managing NetworkManager through DBus, Gorm requires some polkit
permissions. The rules to grant these permissions to users in the `catacomb`
group can be found at [./10-gorm.rules](./10-gorm.rules).
