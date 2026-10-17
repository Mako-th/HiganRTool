# HiganRTool

<!-- zip:skip -->
[日本語](./README.md)
<!-- /zip:skip -->

An **unofficial** online battle tool for Touhou Kaeizuka ~ Phantasmagoria of Flower View (Touhou 9, ver.1.50a).

**It is not affiliated with ZUN or Team Shanghai Alice. Use at your own risk.**

It borrows the unused "妖怪 対 妖怪" entry in Match Mode and adds online battles and spectating to the in-game menu.
Like adonis and Ju.N.Owen, it exchanges both players' inputs so that both games run the same way.
It also has rollback netcode, which earlier tools for this game did not have, so you can play comfortably with less delay.

<!-- zip:skip -->
<img src="images/menu_en.png" width="400" alt="The online menu"> <img src="images/match.png" width="400" alt="A match in progress">
<!-- /zip:skip -->


## Features

- **Random match**: Play someone who chose Random match at the same time, without deciding on a room name.
- **Room match**: Both players enter the same room name to play. It works much like the rooms in Ju.N.Owen.
- **Spectating**: Watch a random match by choosing it from the list, or a room match by entering its room name. When the server is unavailable, you can also watch with a watch code from the host. Up to 10 people can watch. Spectators who join mid-match catch up from the start of the match in fast-forward.
- **Connection codes**: If rooms are not available, you can connect by sending connection codes to each other over Discord or similar. This works like Pure P2P in Ju.N.Owen.
- **LAN**: Play someone on the same LAN. Spectating is also possible. It also works over the virtual LANs of Radmin VPN, Hamachi and ZeroTier.
- **Rollback netcode**: When the opponent's input is late, the game predicts it and keeps going, and goes back and re-runs the frames if the prediction was wrong. It rarely stops even on an unstable connection, and you can play with about 1-2 frames (F) less delay than with adonis. You can also switch to the delay netcode, which waits for the opponent's input like adonis.
- **Change the delay at any time**: Press Ctrl+number key, even during a match.
- **Automatic replays**: Every match is saved to `Replay_HiganRTool\`. File names are in the `th9_udXXXX.rpy` form from the start. Match information is stored inside the replay file, so it is not lost when you move the file.
- **Almost no port forwarding**: The server is used only for matchmaking. Match traffic goes directly between the two players' PCs (UDP). You need an open port only when your opponent is behind a symmetric NAT (→ "When you cannot connect"), and if your router supports UPnP the tool opens it automatically.
- **Encrypted traffic**: Match and spectator traffic is encrypted with a one-time key made for each connection.
- **Automatic updates**: Checks for a new version every time it starts and updates if there is one. After an update it shows what changed. Settings you changed are kept.


## Requirements

- Windows (tested on Windows 11)
- Touhou Kaeizuka ver.1.50a
- A way to talk to your opponent (Discord, etc.)


## Installation

1. Download the latest zip from [Releases](https://github.com/Mako-th/HiganRTool/releases).
2. Extract it **outside the game folder** (e.g. `C:\Games\HiganRTool\`).
3. Run `HiganRTool.exe`. The first time, a window asks for the game folder. Choose the folder that has `th09.exe`.
4. When the game starts, go to Title → Match Mode → 妖怪 対 妖怪 to open the online menu.
5. First, set your name with "Player name" in the menu. It is shown to your opponent and spectators.

### Location and contents

| File | What it is |
|---|---|
| HiganRTool.exe | Start from here |
| HiganRTool.dll | The main part that runs inside the game |
| HiganRTool.ini | Settings (see "Settings" below) |
| HiganRTool_replay_fix.exe | The Replay Repair Tool, which fixes replays saved by v1.0.0 that desync on playback (see "Replay Repair Tool" under "How it works") |
| README.txt / README_en.txt / LICENSE.txt / THIRD_PARTY_LICENSES.txt | This manual (Japanese / English) and licenses |

After the first start, the folder also has `logs\` (logs).


## Starting

From then on, just run `HiganRTool.exe`.
If the game is already running, starting `HiganRTool.exe` exits without starting a second copy.
The tool is not loaded if you start `th09.exe` or vpatch directly. Always start the game from `HiganRTool.exe`.
You do not need vpatch. The tool does the frame timing instead, with a shorter time from input to screen than vpatch.

- To choose the game folder again, clear the `game` value under `[loader]` in `HiganRTool.ini` or start with `--choose-game`.
- If the game does not start, a black window shows the reason and stops. Please tell the developer that line.
- If the game is set to "Run as administrator", set `HiganRTool.exe` to "Run as administrator" too.


## Updates

Every time you start, the tool checks for updates (at most about 2 seconds; if there is no internet connection, it just starts).
If there is a new version, it asks whether to update (the window says 更新しますか？ in Japanese).

- **Yes**: It stops starting, replaces the exe, dll, READMEs, etc., and starts again with the new version. If replacing or restarting fails, it goes back to the old version automatically.
  - The download progress is shown in the black window.
  - The first time the new version starts, it brings `HiganRTool.ini` to the new layout and then shows the change notes. Settings you changed are kept (comments you added yourself are not). Press Enter to start the game. The logs are kept.
  - If the new version causes problems, you can go back to the version before the update (close the game and `HiganRTool.exe`, rename the new `HiganRTool.exe` and `HiganRTool.dll` to other names, and remove `.old` from `HiganRTool.exe.old` and `HiganRTool.dll.old`). The `.old` files are deleted on the start after the new version has started the game once.
- **No**: It starts with the current version. If your version differs from your opponent's, you cannot connect.


## Usage

### Menu

Use ↑↓ to select, and Confirm / Cancel to operate it. A gamepad works too.
The items are listed under "Reading the screen" below.
While waiting, you can close the menu and play other modes.
To stop waiting, choose "Stop waiting" at the bottom of the menu.
"Back" closes the menu.

### Random match

Play someone who chose Random match at the same time, without deciding on a room name.

1. Choose "Random match" at the top of the menu.
2. When an opponent is found, a sound plays and you are connected automatically (→ "After connecting").

While waiting, "Random match waiting" is shown at the top left.
**In a random match, your global IP address is visible to an opponent you do not know.** If that worries you, use a room match or connection codes.
Random matches appear in the Random match list under Watch, and anyone can watch them (to refuse, see "Refuse spectators" under "Settings" below).

### Room match

Two players who enter the same room name play each other.

1. Agree on a room name with your opponent. Use letters, digits, `-`, `_` and `@`, up to 32 characters. Choose a name others are unlikely to use.
2. Both players enter it in "Room name" in the menu, then choose "Room match" under Play.
3. When your opponent arrives, a sound plays and you are connected automatically (→ "After connecting").

While waiting, "Room ... waiting" is shown at the top left.
A room holds only 2 players.

### Connection codes

This connects without the server. Use it when the room server does not respond, for example.

1. One player chooses "Create play code". The code is copied to the clipboard, so send it to your opponent.
2. The other player copies the code they received, then chooses "Paste play code". A reply code is copied to the clipboard, so send it back.
3. The first player copies the reply code, then chooses "Paste reply code".

"Connect code" is the name for the play code and the watch code together.
Codes expire after 30 minutes.
Matches connected with codes can be watched with a watch code (see "Spectating").

### LAN

Two players on the same LAN play each other without the server.
Use it at an offline venue, or for two PCs joined by a LAN cable.

**Host**

1. Choose "Host" under LAN in the menu.
2. Your LAN addresses and ports are listed below the menu (with 4 or more, use ↑↓ to see the rest).
3. Tell your opponent one of those addresses. The connection is made when they enter it.

**Client**

1. Choose "Client" under LAN in the menu.
2. Enter the host's address as `192.168.0.2:17723`. If the port is left out, 17723 is used.
   The address you entered last time is remembered.
3. When connected, a sound plays (→ "After connecting").

Only the first player to join is accepted.
With different hosts, any number of pairs can play on the same LAN at the same time (an experimental feature, not checked on real machines).

**Notes**

- When two PCs are connected directly without a router, Windows may treat that network as public. Then also allow "Public networks" in the firewall prompt for `th09.exe` (→ "Troubleshooting").
- Connecting two PCs directly with a LAN cable, without a router, has not been checked on real machines, but the automatic `169.254.x.x` address should appear on the host's screen.

**VPN virtual LANs**

LAN play and spectating also work over the virtual LANs of Radmin VPN, Hamachi and ZeroTier.
After both players join the same network, connect with the same steps as above.
On the host's screen, VPN addresses carry the tool's name, as in `26.12.34.56:17723 (Radmin)`.
Tell your opponent the address with the name of the tool you both use.
When the VPN tool connects through its relay, the delay increases by that much.
Windows may treat the VPN network as public. Then also allow "Public networks" in the firewall prompt for `th09.exe` (→ "Troubleshooting").

### After connecting

- When both players choose 妖怪 対 妖怪 in Match Mode, the game goes to difficulty select and the match starts. If you were playing another mode, stop at a good point and choose 妖怪 対 妖怪.
- 1P / 2P follow the "Side" setting in "Settings".
- To disconnect, press Cancel on the Match Mode or difficulty screen and choose "Yes".
- When you choose "Save replay and quit" at the end of a match, the connection ends (the replay is already saved automatically).
- When the connection closes after a match, you return to 妖怪 対 妖怪 on the title screen. To connect again, choose it there.
- If the opponent does not respond for 60 seconds, the connection is closed (your game does not stay frozen either).
- When the opponent stops responding for about 2 seconds, "Waiting for the opponent" appears at the top left. Hold Esc for 3 seconds while it is shown to disconnect yourself.

### During a match

- **Change the delay**: Ctrl+number key on the top row. 0-8 set the value and 9 sets auto. Both players can change it at any time.
- **Change the netcode**: Ctrl+Tab on the character select screen. Both players can change it.
- **Copy the watch code**: Ctrl+C. The host presses it once the two players have connected. Whether it was copied is shown at the top left for a few seconds.
- **Random character / map, skip dialogue**: Ctrl, or the skip button on a gamepad (as in the game). Ctrl+number and Ctrl+Tab also count as pressing Ctrl. If either player presses Ctrl, the dialogue is skipped for both.

### Spectating

Up to 10 people can watch each random match and each room match.
If you join mid-match, you catch up from the start of the match in fast-forward.
Spectators do not change the delay of the two players.
If you watch a match to the end, the replay is saved on your side too.
Press Esc to stop watching.
To refuse spectators, see "Refuse spectators" under "Settings" below.

**Random match**

1. Choose "Random match" under Watch in the menu. The random matches in progress are listed as "1P name vs 2P name   spectators/limit".
2. Choose the match you want to watch (full matches are greyed out; if there are more than 8, scroll with ↑↓).
3. You can watch the match between the two players.

**Room match**

1. Ask the two players for their room name.
2. Enter it in "Room name" in the menu, then choose "Room match" under Watch.
3. The match appears automatically when the two players start.

Anyone who knows the room name can watch.

**Watch code**

Use this when you cannot reach the server.

1. Once the two players have connected, the host presses Ctrl+C. The watch code is copied to the clipboard, so send it to the spectator.
2. The spectator copies the watch code, then chooses "Paste watch code" under Connect code in the menu.

The host is the player whose top-left message says "(host)" when connected.
With a watch code, the spectator must be able to reach the host directly.
Make sure you are on the same LAN, or that the host's UPnP works.

**LAN**

1. After the two players have connected, the spectator chooses "Watch" under LAN in the menu.
2. Enter the host's address to watch.

### Reading the screen

**Menu**

<!-- zip:skip -->
<img src="images/menu_guide_en.png" width="640" alt="What the menu shows">
<!-- /zip:skip -->

1. **Play**: Random match (play without deciding on a room name) and Room match (play with a room name). The cursor starts on Random match when the menu opens.
2. **Watch**: Random match (pick from a list) and Room match (enter the room name).
3. **Connect code**: Create play code, Paste play code (while waiting for the reply: Paste reply code) and Paste watch code. Connect without the server (a backup).
4. **LAN**: Host, Client and Watch. Connect inside the same LAN without the server.
5. **Settings**: Change your player name, side, netcode, delay and language. The menu shows one "Settings" row, which opens a screen one level down.

If your player name is the default (`HiganRTool`), a note is shown below the menu.
Only the first time you open the menu after starting, the cursor is on "Settings", so set your player name there.
While waiting, "waiting 0:42" is shown to the right of the row you are waiting on.

**During a match**

<!-- zip:skip -->
<img src="images/match_guide.png" width="640" alt="What the match screen shows">
<!-- /zip:skip -->

1. **Player names**: 1P on the left, 2P on the right.
2. **Delay**: The current delay (→ "How it works", "Delay"). The number in brackets is a guide based on ping, and "(A)" means auto is in use.
3. **ping**: The round-trip time to your opponent.
4. **rollback**: The largest number of frames rolled back in the last second. Shown only with rollback netcode.

On the character select screen, the netcode is also shown at the bottom left.


## Settings

- **Player name**: The name shown to your opponent and spectators. It appears at the top left and right during a match.
- **Room name**: The name used for room matches and spectating. It is used again at the next start.
- **Side**: Any / 1P / 2P. If one player chose Any, the other player's choice is used. If both want the same side, or both chose Any, it is random.
- **Netcode**: rollback (default) / delay. Rollback is used only when both players choose it.
- **Delay**: auto / 0F-8F. Auto picks the value from ping (→ "How it works", "Delay").
- **Language**: 日本語 / English. At first, it follows the Windows display language. Logs are always in Japanese.

Settings you change in the menu and the game location you choose are written to the matching line of `HiganRTool.ini` in this tool's folder as soon as you decide them.
`HiganRTool.ini` and the menu always show the same value; neither takes priority.
For settings not in the menu, edit `HiganRTool.ini`.

- **Lower the processing load**: Add `auto_window = b-hybrid` under `[pacing]` to cut the processing load to about a quarter, at the cost of a tiny bit more delay (about 0.2 ms).
  Try this if you play in windowed mode while streaming, or when your PC is busy.
- **Refuse spectators**: Both players set `enable = 0` under `[spectate]` (some spectating follows the host's setting, and which player is the host is not decided in advance). Random matches are not listed either.
- **Turn off automatic updates**: Set `update_check = 0` under `[loader]`.

**How to edit `HiganRTool.ini`**
- A line like `[replay]` is a category. The `key = value` lines below it, such as `enable = 1`, are the settings of that category.
  Write each key under the category shown in the table below.
- Lines starting with `;` or `#` are commented out and not used.
  The `HiganRTool.ini` in the zip has some lines commented out with `;`. To use one, delete the `;` at the start of the line.
- After editing, save the file and start this tool again. `HiganRTool.ini` is read only when the tool starts.

Example: make the window 1280x960 (delete the `;` from these lines at the end of `HiganRTool.ini` in the zip)

```
[window]
width = 1280
height = 960
```

A line with an unknown key or an unreadable value is ignored on its own, and the default is used.
The ignored lines are shown below the netplay menu (the full list is written to the log).
You can put a comment after a value, after a space and a `;`.
A `HiganRTool.ini` saved in Shift_JIS instead of UTF-8 is also read.

All settings are listed in the table below.

| Category | Key | Meaning |
|---|---|---|
| [netplay] | name | Player name |
| | room | Room name |
| | lan_host | The host address last entered under LAN "Client" or "Watch" (`192.168.0.2` or `192.168.0.2:17723`) |
| | mode | Netcode. `rollback` (default) / `delay` |
| | delay | Delay. `0`-`8` / `auto` (default; shared by rollback and the delay netcode) |
| | host_seat | The Side. `1` / `2` (Any if there is no line) |
| | rollback_hit_se | Hit sound with rollback. `confirmed` (only when confirmed; default) / `live` (already when predicted) |
| | ipv6 | Whether to use IPv6. `on` (default) / `off` (do not use) |
| | stun / stun2 | Servers asked for your outside address. Empty means no query |
| | enable / scope / code_side / room_server / check | Basic network settings. Do not change |
| | bind | The UDP port to listen on (default `auto:17723`; `auto:` tells the tool to pick your own address automatically). You may change it to a port you can open (forward). The two players do not need to use the same value |
| | upnp | Whether to ask the router (UPnP) to open the bind port. `on` (default; only while waiting and during a match, closed afterwards) / `off` (do not use) |
| [pacing] | mode | How frames advance. `auto` (in windowed mode, the tool keeps 60.000 fps) / `game` (left to the game) |
| | auto_window | How the wait is done in windowed mode. `b-spin` (default) / `b-hybrid` (lighter processing load; see "Lower the processing load" above) |
| [replay] | enable | Automatic replay saving. `1` (save each match) / `0` (do not save). The save location is under "How it works", "Replays" |
| | layout | How the save folder is split. `peer` (`<1P> vs <2P>` folders) / `date` (by date) / `flat` (not split) / `pack50` (50 per folder, with names you can copy straight into the game's `replay` folder) |
| [spectate] | enable | Accept spectators. `1` (accept; default) / `0` (refuse). Room match, watch code and LAN spectating follow the host's value. Random matches are not listed if either player has `0` |
| [window] | width / height | Window size in pixels of the game image |
| | x / y | Position of the window's top-left corner (only when both are written) |
| | titlebar | Frame and title bar. `1` (show) / `0` (hide) |
| | alwaysontop | Always on top. `1` (in front of other windows) / `0` (no) |
| [pad] | pov | Whether to read the gamepad's D-pad as directions. `1` (read; default) / `0` (do not read) / `always` (for diagnosis). For gamepads the game does not read |
| [ui] | lang | Language of the tool's screens. `auto` (Windows display language) / `ja` / `en` |
| [log] | detail | Amount of logging. `0` (brief; default) / `1` (detailed; use it when a problem occurs) |
| [loader] | game | The location of the game's `th09.exe` (if empty, a window to choose the folder opens) |
| | update_check | Update check before starting. `1` (check; default) / `0` (do not check) |

If `[window]` is not written, the window stays as before. No effect in fullscreen.


## How it works

**Delay**
- Delay N (shown as "Delay NF" on screen) means your input is delayed by N frames (N x 16.7 ms) so that both players can play on the same frame.
- auto picks the value from ping: about 1F per 33 ms of round trip with the delay netcode, and about 1F per 67 ms with rollback (half of the one-way frames). When the ping is low and stable, it picks 0F. It is reviewed only at the start of a match and when you press Ctrl+9, never in the middle of a match.
- To avoid changing every match, it goes up only when the round trip passes the boundary by more than 5 ms, and goes down only after 2 matches in a row were lower (pressing Ctrl+9 applies it at once).

**rollback (default)**
- For frames where the opponent's input has not arrived yet, the game predicts "the same input as before" and keeps going. When the real input arrives and differs, it goes back to that frame and runs again. So you can play at a lower delay than with a waiting netcode without stops.
- When a prediction is wrong, bullets or the opponent may jump.
- The two games compare their random state and life regularly, and if they differ, the match is stopped and a sound plays.

**delay netcode**
- Waits until the opponent's input arrives (the same as adonis). The screen keeps drawing while waiting.

**Network**
- Match traffic goes directly between the two PCs (UDP), not through a server. Usually you do not need port forwarding.
- Only when your opponent is behind a symmetric NAT does your port need to be open. If your router supports UPnP, the tool opens it automatically (→ "When you cannot connect").
- The server is used only to find your opponent in a room and to ask for your outside address (STUN).

**Replays**
- One replay is saved automatically after every match, to `Replay_HiganRTool\<1P> vs <2P>\` in the game folder (if the game is in Program Files, Windows saves it to the same place in VirtualStore).
- Nothing is written to the game's `replay` folder. To watch one, copy it into the `replay` folder.
- The game has a bug that crashes when saving the replay of a long match. This tool makes the game estimate the needed size correctly before saving, so it does not crash.

**Replay Repair Tool (HiganRTool_replay_fix.exe)**
- **What it fixes**: Replays saved by v1.0.0 or earlier.
  Replays saved by v1.1.0 or later, and replays already fixed by this tool, are skipped as "need no fix".
  Replays not saved by HiganRTool cannot be fixed.
- **How to use**:
  1. Double-click `HiganRTool_replay_fix.exe`.
  2. Choose with "Choose replays" or "Choose a folder". You can also drop them onto the exe or its window.
     When you choose a folder, all replays inside it (up to 4 levels down) are loaded.
  3. Each replay shows the 1P and 2P names and the charge settings (slow / charge) stored in it. Switch with "Prev" and "Next".
  4. Under "New", choose the settings the two players actually used in that match.
  5. "Fix this replay" fixes the replay on screen; "Fix the whole folder the same way" fixes every replay in the same folder with the same settings.
  6. When it finishes, it shows how many were processed, where they were saved, and which were skipped.
- **Where fixed replays go**: A copy with the same file name is made in `fix_replay\<original folder name>\<1P setting>-<2P setting>\`, one level above the original folder.
  For example, fixing `Replay_HiganRTool\Alice vs Bob\th9_ud0008.rpy` with 1P charge / 2P slow makes `Replay_HiganRTool\fix_replay\Alice vs Bob\charge-slow\th9_ud0008.rpy`.
  The original files are not changed.
  Existing files with the same name are not overwritten.
- **Which settings to choose**: Choose the settings the two players actually used in the match, not your PC's setting.
  If you are not sure, make both and play both to compare.
  `Replay_HiganRTool` has one folder per `<1P> vs <2P>`, and 1P and 2P are in the same order within a folder.
  If the opponent did not change their setting, you can fix that folder all at once.
- **Watching in the game**: Copy the fixed replays into the game's `replay` folder.
  Checking them in the game and replacing the replays that go out of sync are up to you.
- **Window language**: Switch between 日本語 and English with "Language" at the top right.

**What the tool does to the game**
- It starts the game suspended, loads the DLL, and changes only fixed places in memory. Before changing them, it checks that they match ver.1.50a, and changes nothing if they do not.
- It never changes files in the game folder (exe, settings, save data, replays). It writes only inside `Replay_HiganRTool`.
  During online play, you can still take a screenshot with P / Home (the game itself saves it to its usual `snapshot` folder). Pressing them does not take one on the opponent's PC.


## Safety

- **Impersonation**: All match traffic carries an authentication tag made with a secret key for each connection. If someone who does not know the connection code sends fake inputs or disconnects, they are dropped without being read.
- **Contents**: The contents of match and spectator traffic (inputs and names) are encrypted with a one-time key made for each connection. Even the developer, who runs the room server, cannot read them.
- **What your opponent sees**: Your global IP address (spectators see the host's; anyone can watch a random match from the list, so spectators you do not know can see it too). It is also inside the connection code. Play only with people you trust, and do not give connection codes or logs to people you do not know.
- **The room server**: Run by the developer on Cloudflare. It receives room names and connection codes over encrypted traffic (HTTPS), passes them to the other player and keeps no records. However, since connection codes pass through the server, the design cannot stop the server from handing over a fake code and sitting in between. If that worries you, send connection codes to each other by hand.
- **UPnP**: While waiting and during a match only, the tool asks your home router to open the bind port (inside your home network only).
  - It closes the port afterwards. If closing fails, the mapping expires after 1 hour.
  - For routers that refuse mappings with a time limit, it asks for one without a limit. If the tool crashes then, a mapping named "HiganRTool" stays, but the next start takes it over and removes it when the tool next exits. You may also remove it by hand in the router settings.
  - While open, traffic from outside can reach that port, but anything with a wrong authentication tag is dropped (→ "Impersonation"). To turn this off, set `upnp = off` in `HiganRTool.ini`.
- **Outgoing queries**: Once per start, the update check (the room server, and the GitHub download source it points to; only a query is sent). On the first connection, one query each to STUN (Cloudflare and Google). Otherwise, if you only play without opening the menu, nothing is sent.
- **Updates**: The hash list of the update files is signed by the developer. A list with a wrong signature, a version older than the current one, or a file whose hash (SHA-256) differs from the list is not used.


## Troubleshooting

- Please tell the developer about bugs.
  Contacts: GitHub Issues, Twitter (reply directly to @Sinus_lunicus_), or Discord (message makocchan. directly).
- **Cannot connect**: Look up the number on the last line of the warning at the top left in "When you cannot connect" below.
- **"HiganRTool version differs from the opponent"**: Both players should use the same zip.
- **"Desync detected. This match was stopped."**: Press Esc to go back to the menu. Both players, please attach the logs from the `logs` folder and tell the developer.
- **A firewall prompt about `th09.exe`**: Check "Private networks" and click "Allow access".


## When you cannot connect

If you cannot connect, the cause and a number appear at the top left.

A **symmetric NAT** is a router that uses a different outside port for each destination (common in university and company networks and some mobile connections).
From behind this type of router, you cannot connect unless the other side's port is open.

1. **UDP packets from the opponent do not arrive, for an unknown reason**: Record detailed logs by following the steps under "Logs" below, try again, then both players attach the logs and tell the developer. You also see this when the network blocks UDP (some universities, etc.).
2. **Your opponent is behind a symmetric NAT and your UPnP is not available**: Open your port. Do one of these:
   - (a) Turn on UPnP in the router settings (this also lets other apps in your home open ports).
   - (b) Forward a UDP port on the router and set `bind` in `HiganRTool.ini` to that number. Example: for `bind = auto:17723`, forward UDP 17723 to this PC. You may close it after playing.
3. **You are behind a symmetric NAT**: It cannot be fixed on your side. Ask your opponent to do 2 (a) or (b). If your opponent is also behind a symmetric NAT, you cannot connect (same as 4).
4. **Both players are behind symmetric NATs**: There is no way to connect directly. One of you should connect from another network (a home connection, etc.).
5. **Packets arrive from the opponent but you cannot connect**: If one connection code is old, packets may arrive in one direction only. Both players choose "Stop waiting", and one creates a new code and sends it at once (in a room match or a random match, stop waiting and switch to "Create play code" under Connect code). If it still does not connect, send the logs as in 1.


## Logs

- Logs are kept in the `logs` folder, one file per day (`dll_YYYYMMDD.log`). Starting several times on the same day adds to the same file, with a "==== 起動 ..." line at each start. Files older than 3 days are deleted automatically.
- Send only that day's `dll_YYYYMMDD.log` (normally the brief log, just this one file).
- If the game crashes at start-up or the window closes immediately, also attach `loader_YYYYMMDD.log` (a record of the start-up steps) from the `logs` folder.
- Only when asked for a detailed record, change `detail = 0` under `[log]` in `HiganRTool.ini` to `detail = 1` (in an old `HiganRTool.ini` without `[log]`, add the two lines `[log]` and `detail = 1`), and play again.
  Also send the file ending in `_net_frames.csv.gz` (`_watch_frames.csv.gz` when watching). This file gets a separate name for each start. Send the .gz as it is, without opening it.
- Logs do not contain names, Windows user names or raw IP addresses (only the kind of IP, such as "outside" or "home", and port numbers). Even so, please send them only as part of a bug report (contacts are in "Troubleshooting"), and avoid posting them as-is where anyone can see them.


## Limitations

- Only ver.1.50a is supported.
- Two players who are both behind symmetric NATs, or a connection that blocks UDP, cannot connect. This needs a relay server, and there is no plan for it yet.
- From mainland China, the room server (Random Match, Room Match and the spectate list) may be unreachable without a VPN. Connection codes, LAN play and watch codes do not use the server and still work.
  You can also play a distant opponent with LAN play if you both join a Radmin VPN, Hamachi or ZeroTier virtual LAN (→ "LAN").
- Under Linux (Wine), the launcher may time out while waiting for the DLL to load. Run `wineserver -k` and start it again.
- It cannot be used together with other tools for this game (other netplay tools, practice tools), because they patch the same places and conflict.
- Up to 9999 replays are saved in one folder. After that, replays are not saved, so move or delete old ones.
- Replays saved automatically by v1.0.0 desync when played back in the game if the two players used different charge settings (slow / charge).
  The netcode (rollback / delay) does not matter.
  The match itself was played with the correct settings; only the replay playback desyncs.
  From v1.1.0, replays are saved with the correct settings.
  How to fix replays saved by v1.0.0 is under "Replay Repair Tool (HiganRTool_replay_fix.exe)" in "How it works".


<!-- zip:skip -->
## Changelog

Changes in each version are listed in [CHANGELOG.en.md](CHANGELOG.en.md).
<!-- /zip:skip -->


## About the use of AI

The code of this tool was written by AI (Claude).
The technical document (TECHNICAL.md on GitHub) was also written by AI, so that I and outside engineers can check and understand the design.
That is also why the tool is open source: to make the design public.

I decide what to build, how it is designed, and what to prioritize (such as minimizing latency and the security design).
I also make the decisions that come up during implementation, such as how it behaves, the on-screen wording, the controls, and how latency is handled.
In addition, when testing with the actual game, I play real matches myself and with friends, and read the logs to decide what needs to be fixed.

I referred to how other netplay tools such as adonis and Ju.N.Owen work, but did not copy their code.

I have a basic knowledge of computers, but I am still learning the standard approaches to rollback, netcode for competitive games, and network protocols, through the technical document, web searches and advice from friends, so there may be places where my understanding or the implementation falls short.
It comes with no warranty, as stated in the license, but I take responsibility for this tool and intend to keep maintaining it as far as I can.
Thank you for your understanding.


<!-- zip:skip -->
## For developers

The specification (sync, networking, connections, the room server, logs) is explained in [TECHNICAL.en.md](TECHNICAL.en.md).

### Build

Requirements: [Rust](https://rustup.rs/) (`rustup target add i686-pc-windows-msvc`) and Visual Studio Build Tools (used as the linker).
The game is 32-bit, so the build targets 32-bit Windows (set in `.cargo/config.toml`).

```
cargo build --release
```

This produces `higanrtool_loader.exe` (launcher) and `higanrtool.dll` (main part) in `target\i686-pc-windows-msvc\release\`.
Rename them to `HiganRTool.exe` and `HiganRTool.dll` and put them in the same folder.

- Your own builds cannot connect to the released zip (the build number is compared before connecting). Your own builds can connect to each other.
- The room server (`src/server`) runs on Cloudflare Workers.
- The tests in the source are run in the development environment (some data they read is not in this repository).
<!-- /zip:skip -->


## License

<!-- zip:skip -->
[GPL-3.0](./LICENSE) (version 3 or later).
<!-- /zip:skip -->
<!-- zip:only
GPL-3.0 (with an exception for Team Shanghai Alice; see `LICENSE.txt`).
-->
Team Shanghai Alice may use, modify and redistribute this software without being bound by the terms of the GPL.
<!-- zip:only
Licenses of other software bundled with this tool are in `THIRD_PARTY_LICENSES.txt`.
-->


<!-- zip:skip -->
## Author

Mako-th

<https://github.com/Mako-th/HiganRTool>
<!-- /zip:skip -->
