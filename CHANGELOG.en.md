# Changelog

[日本語](./CHANGELOG.md)

Different versions cannot connect to each other.
Use the same version as your opponent.


## v1.1.1

### Fixed

- Fixed LAN play and spectating not working over the virtual LANs of Radmin VPN, Hamachi and ZeroTier.
  On the host's screen, VPN addresses carry the tool's name, such as "(Radmin)".
- The LAN host's waiting screen now also shows the addresses of virtual adapters (such as VMware).
  Real LAN addresses come first, and up to 3 are shown at a time. With 4 or more, use ↑↓ to see the rest.
- When both players want the same side, it is now decided at random instead of first come, first served.
- While "Waiting for the opponent" is shown, you now disconnect by holding Esc for 3 seconds.
- Fixed replays recording the delay from before the change in matches where the delay was changed right after connecting.
  It now records the delay used the longest in the match.
- Fixed the detailed log (`.csv.gz`) sometimes being cut off when the game was closed.
- Some on-screen wording and menu messages were fine-tuned.

### Logs

- Each connection (the menu item chosen, whether you are the host, and the waiting time) and spectators joining and leaving are now recorded.
- When the update check is skipped at startup, the reason is now recorded.
  When `update_check` in `[loader]` of `HiganRTool.ini` is 0, it is also shown on screen.

### Documents

- The README was reorganized, and the zip's `README.txt` now has the same content.
- Added the "About the use of AI" section.


## v1.1.0

### New

- The Replay Repair Tool (`HiganRTool_replay_fix.exe`), which fixes replays saved by v1.0.0, is included in the zip.
- LAN play.
  With "Host", "Client" and "Watch" under "LAN" in the menu, you can play and spectate with people on the same LAN without the server.
- Matches connected with a connection code can now be spectated.
  Once the match is connected, the host copies a watch code with Ctrl+C and sends it to the spectators.
  Spectators enter it in "Paste watch code" in the menu.
- Ctrl now picks characters and maps at random and skips the dialogue after a match is decided.
  It works on online select screens and in matches, as does the skip button on a gamepad.
  The R key for random select has been removed.
  Note that changing the delay (Ctrl+number) on character select also moves the character at random.
- When a match ends (the connection is closed), you return to 妖怪 対 妖怪 on the title screen.
- During online play, P / Home take screenshots (nothing is saved on the opponent's PC).
- The update display has changed.
  It shows the download progress, and what changed after updating.
  `HiganRTool.ini` is brought to the new layout and the settings you changed are kept.


### Fixed

- Saved replays desynced on playback when the two players used different charge settings (slow / charge).
- With a `d3d8.dll` in the game folder (such as ENBSeries DX8 to DX9 Convertor), the game crashed when 妖怪 対 妖怪 was chosen.
- On character select, the opponent's cursor drifted when the two players had different characters unlocked, or during random select.
- Keyboard input did not work when Player 1's input device in the game's key config was set to a gamepad.
- On Linux (Wine), Room Match, Random Match and the spectate list did not work.
- When the opponent's game froze, your game stayed frozen too.
  It now disconnects after 60 seconds without a response.
- Starting `HiganRTool.exe` several times in a row started several copies of the game.
  If the same game is already running, it now exits without starting anything.
- When connecting, the window frame flashed as well; now only the taskbar button flashes.
- Late spectators now catch up without stalling.
- A line in `HiganRTool.ini` that cannot be read falls back to its default; the rest still works.

### Changed

- Player name, side, netcode, delay and language are now under "Settings" in the menu.
- With rollback and auto delay, a lower value is picked for the same ping.
  It picks 0F when the ping is low and stable.
- The wait for the DLL to load was raised from 10 to 60 seconds.
- The logs now record startup crashes (`loader_YYYYMMDD.log`) and which `d3d8.dll` is loaded.


## v1.0.0

First public release.

- Online play with Random Match, Room Match and connection codes.
- Spectating Random Match and Room Match (up to 10 spectators).
- Rollback netcode, and the delay netcode used by adonis.
- Changing the delay during a match.
- Automatic replay saving.
- Encrypted match and spectator traffic.
- Automatic updates.
