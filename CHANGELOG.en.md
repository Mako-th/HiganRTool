# Changelog

[日本語](./CHANGELOG.md)

Different versions cannot connect to each other.
Use the same version as your opponent.


## v1.2.1

### Added

- At the end of a match, each player now chooses and decides on the menu.
  The item each player is choosing is outlined in red for 1P and blue for 2P.
  When the two chosen items differ, the lower one is used.
  You can cancel your decision until both players have decided.
  However, if either player decides on "Save replay and quit", the connection ends right away.
- "1P" and "2P" in the name display during a match are now shown in red and blue.
  This matches the outline colors of the end-of-match menu.
- In Match Mode's "人間 対 人間", 1P and 2P can now each play on their own keyboard.
  Each time you choose "人間 対 人間", 1P and 2P each press the keys shown on screen (Z, X, Shift, Up, Down, Left, Right) on their own keyboard to assign it.
  After assigning, input from anything other than the assigned keyboards (including gamepads) is ignored until the match ends or you go back to a menu before "人間 対 人間".
  Press a gamepad button or Esc while the prompt is shown to play without splitting that time.
  Set `split_keyboards = 0` under `[input]` in `HiganRTool.ini` to skip the prompt and keep playing without splitting.
- Added `status_side` (where the status and delay are shown during a match) to `[ui]` in `HiganRTool.ini`.
  Choose from `1p` (bottom left, default) / `2p` (bottom right) / `self` (your side) / `opponent` (the opponent's side).
- You can now set the file name of saved replays with `name` under `[replay]` in `HiganRTool.ini`.
  You can use `{date}` (yyyymmdd), `{time}` (hhmm), `{1Pchara}`, `{2Pchara}` (character names), `{1Pname}`, `{2Pname}` (player names), `{wins}` (rounds won), `{rlifes}` (life at the end of each round) and more.
  The default is `th9_ud{num}`, the same as before.
- If a new version is released while you are playing, it is now shown on screen.
  It appears below the menu that opens when you choose 妖怪 対 妖怪, and at the top left of the end-of-match menu while connected.
  Set `update_check = 0` under `[loader]` in `HiganRTool.ini` to turn it off.

### Changed

- A room name now cannot be used by anyone else while its match is running.
  Trying to enter a name in use shows "This room name is in use."
  The name becomes available again when the connection to the room in use ends.
- More symbols can now be used in player names and room names.
  `` ` ``, `;` and `/` cannot be used.
  Room names starting with `w.` or `q.` cannot be used either.
- Only the two players who play in a room can now accept spectators and list the match in the spectator list.
  This keeps people who are not playing from creating the spectating room first and making arriving spectators connect to their PC.
- Reviewed the security of the network code and the room server with audit tools.
  The results and what the design cannot protect are in "11. Security" of [TECHNICAL](./TECHNICAL.en.md).
- Enabled Control Flow Guard (CFG) for `HiganRTool.exe`.
  ASLR, DEP and SafeSEH were already enabled; they are now also stated explicitly in the build settings.
- Logs are now sent to the developer as a zip of the `logs` folder.
  The messages on screen and the Issue templates were updated to match.
- Rewrote the comments in the source code to make them easier to understand.

### Fixed

- Fixed a bug where a malicious spectating host could make a spectator's PC take screenshots or create many empty folders.
- Fixed a bug where a PC running HiganRTool could be made to send packets to an unrelated third party.
  Added limits on link measurement, spectating requests and LAN spectate-code replies.
- Fixed a bug where control characters in the opponent's name from a connection code were shown as is in the `HiganRTool.exe` window.
- Fixed a bug where replays were not saved automatically when the game folder name contained Chinese or other non-Japanese characters.
  When run as administrator, they were saved into a folder with a garbled name.
  Please move replays saved in that folder manually.
- Fixed a bug where jitter in the connection alone could make the host stop sending to spectators, freezing the spectated match.
- Fixed a bug where letters and digits in the status and delay row during a match were cut off or spaced out on Windows without MS Gothic (such as English Windows).
  Without MS Gothic, letters and digits are now shown in a monospaced font.
  In addition, fonts that are standard on Wine and Proton on Linux and on Wine on Mac were added to the fonts used for Japanese text when MS Gothic is not available.
- Fixed a bug where replays were not saved automatically after a folder was full up to `th9_ud9999`.
  They are now saved in a folder with the same name plus `_2`.


## v1.2.0

### Added

- Added `rollback_cutin_se` under `[netplay]` in `HiganRTool.ini` (when to play the cut-in sound while using rollback).
  The default is `confirmed`, which holds the sound until the frame is confirmed, like the hit sound.
  The more rollback frames, the later your own cut-in sound plays, as it also waits until confirmed (up to about 130 ms). To play it at prediction time, set `live`.
- Pressing Esc during a match while spectating now opens the game's pause menu.
  While the pause menu cannot be opened, "Paused" is shown at the top left.
- When watching random matches and room matches, spectators now relay the match to each other.
  The host basically sends directly to up to 2 spectators, which reduces the load on the connection.
  If you do not want to relay to other spectators, set `relay = 0` under `[spectate]` in `HiganRTool.ini`.
- Added `ff_max` under `[spectate]` in `HiganRTool.ini` (fast-forward speed when you join mid-match; `1`–`16` times, default `8`).

### Changed

- Added `vsync` under `[pacing]` in `HiganRTool.ini`.
  Choose from `off` (default) / `on` / `game` (the custom.exe setting as is). The custom.exe setting is not rewritten.
- Renamed `auto_window` under `[pacing]` in `HiganRTool.ini` to `wait` (`passive` / `active`), and made the lighter `passive` the default.
  Your current setting is carried over on the automatic update.
- The tool now keeps 60.000 fps in fullscreen without vsync as well.
- Vsync in fullscreen is now off by default (the tool keeps 60.000 fps).
  To follow the custom.exe setting, set `vsync = game` under `[pacing]`.
- The tool now works around a game bug where saving a Story, Extra or Match replay of a long game could crash the game. (Gameplay is unchanged, but the game's behavior when saving a replay is changed.)
- Replays saved outside 妖怪 対 妖怪 now also carry a mark that the game was started from HiganRTool (the tool's version).
  Normal modes can be played and their replays saved just as when starting the game on its own, so the mark lets you tell them apart.
- Stopping spectating is now done by holding Esc for 3 seconds.
  This is to tell it apart from the Esc that opens the pause menu. You can also stop with "Return to title" in the pause menu.
- Spectating connections now also use IPv6.
- Spectators who join mid-match now catch up faster (8x by default).
- When you cannot watch because the match is full of spectators, this is now shown on screen.
- The host that sends the match to spectators is now chosen automatically as the player who is easier to connect to and has the faster upload.
- When the host's sending to spectators could slow down the match traffic, the host now sends less to spectators and gives priority to the match.
- Spectators keep receiving the match from the host even while the game is stopped, such as while the window is being dragged.
- Spectating now ends when the records buffered on the spectator's PC reach the limit (about one hour).
- Reworded the log messages to be shorter and easier to read.
- The GitHub repository now runs the tests with GitHub Actions on every push and pull request.
- Dependency updates are now checked weekly with Dependabot and taken in only after the tests pass.

### Fixed

- Fixed a crash that could happen when starting with the fullscreen setting.
- Fixed the heavy processing load while another window was in front of the game in fullscreen.
- Fixed thprac not recognizing the game started from HiganRTool.
- Fixed Discord's game capture treating the game started from HiganRTool and the one registered with vpatch as different games.
- Fixed HiganRTool.exe not starting on older Wine, such as CrossOver on Mac.
- Fixed another copy of the game being able to start while the one started from HiganRTool was running.
- Fixed matches being watchable in room matches, connection-code matches and LAN even when the opponent did not allow watching.
  If either player has `enable = 0` in `[spectate]`, the match cannot be watched.
  In a room match, the spectator is now told right away that the match cannot be watched.
- Fixed spectators not receiving the match over the internet in some cases.
  This could happen when the match was connected over IPv6, or when the spectator's PC clock was off.


## v1.1.1

### Fixed

- Fixed LAN play and spectating not working over the virtual LANs of Radmin VPN, Hamachi and ZeroTier.
  On the host's screen, VPN addresses carry the name of the VPN you use, such as "(Radmin)".
- The LAN host's waiting screen now also shows the addresses of virtual adapters (such as VMware).
  Real LAN addresses come first, and up to 3 are shown at a time. With 4 or more, use ↑↓ to see the rest.
- When both players want the same side, it is now decided at random instead of first come, first served.
- While "Waiting for the opponent" is shown, you now disconnect from the opponent by holding Esc for 3 seconds.
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
  Note that changing the delay (Ctrl+number) on character select also triggers the Ctrl random select and moves the character.
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
  The tool now disconnects after 60 seconds without a response from the opponent.
- Starting `HiganRTool.exe` several times in a row started several copies of the game.
  If the same game is already running, it now exits without starting anything.
- When connecting, the window frame flashed as well; now only the taskbar button flashes.
- Late spectators now catch up without stalling.
- A line in `HiganRTool.ini` that cannot be read falls back to its default; the rest still works.

### Changed

- Player name, side, netcode, delay and language are now under "Settings" in the menu.
- With rollback and auto delay, a lower value is picked for the same ping.
  It picks 0F when the ping is low and stable.
- The time HiganRTool.exe waits for the DLL to load was raised from 10 to 60 seconds.
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
