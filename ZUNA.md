# GPUI for Zuna

This branch is Zed's v1.27.0 release with the changes [Zuna's](https://github.com/SocketByte/zuna-chat) native desktop app (`apps/desktop`) needs in GPUI on top, one commit each. The app takes `gpui` and `gpui_platform` from it as a git dependency pinned to a commit, so nothing changes for it until that pin moves. `main` follows Zed's and carries none of this.

## The changes

- **`gpui: Add TextLayout::closest_index_for_position`.** The character boundary nearest a point, as where a selection made with the mouse ends, for a text element's lines.
- **`gpui: Add Window::cursor_style_under_mouse`.** The cursor what's under the pointer asks for, so the app can leave a double-click on something clickable alone.
- **`gpui: Activate a Wayland window with a token another program handed over`.** `Window::activate_window_with_token`, for a second start's `XDG_ACTIVATION_TOKEN` and the tray's click, as Wayland only brings a window forward with one.
- **`gpui: Read files a file manager copied from the Wayland clipboard`.** `App::read_paths_from_clipboard` reads the `text/uri-list` a file manager offers. Zed's own version, for X11 too and through `ClipboardEntry::ExternalPaths`, is zed-industries/zed#66625: once it's in a release, this goes and the app reads that.
- **`gpui: Decode every frame of an animated WebP held in memory`.** `Image::to_image_data` decoded only a WebP's first frame, so an animated emote the app fetched itself stood still. Zed's main decodes them all (`decode_webp_image`): this goes once a release has it.
- **`gpui: Count what a cached view's own draw touched`.** `App::detect_accessed_entities` left out of a cached view's set whatever anything earlier in the frame had touched, so a view drawn earlier in the frame left itself out, and notifying it drew nothing once its drawing was reused: a chat opened before its first message never showed it. Zed's main counts it in scopes of its own: this goes once a release has that.
- **`gpui: Draw an animation's next frame when it's due, on the image's own clock`.** `RenderImage::frame_at`: every copy of an animated image shows the same frame, as in a browser, and its view is drawn again when the next frame is due rather than on every vsync. It keeps moving while the window is visible but not focused.
- **`gpui: Let go of images that have been out of every frame for a while`.** Images out of every window's frames for a minute leave the atlases, and those an `img` decoded from an `Image` leave the asset cache too, with 128 MB of decoded images out of view at most.
- **`gpui: Glide a mouse wheel's notches, as Chromium does`.** 100 px a notch in a list or a scrolling area, eased out over about 150 ms. A touchpad's pixels move at once.
- **`gpui_windows: Draw frames only when a window wants one`.** Windows give GPUI a frame waker, and the vsync thread sleeps until something wants a frame, rather than waking the UI thread at the display's refresh rate for as long as the app runs. The first frame after a rest is drawn at once.

## What GPUI 0.2.2 needed and this release doesn't

Zuna carried GPUI 0.2.2 from crates.io in its own tree before this branch. These of its changes went, as v1.27.0 has its own: one Direct3D device at startup, an animation's frame kept to its own picture, Wayland asking for frame callbacks only when wanted (its `FrameLoop`), a copy made with a click on Wayland (`selection_serial`), a frame where the compositor draws none on Wayland, a tooltip's own delay (`tooltip_show_delay`), and the second of presenting the same scene after input. The patched `xattr` went too: this release uses xattr 1.x.

## Moving to a newer release

Make a branch for it from its tag, named as this one is (`zuna/v1.28.0`), and cherry-pick these commits onto it, fixing what conflicts. Drop a commit once the release has its own fix, and say so here. Check in the app as its `docs/WINDOWS_APP.md` and `docs/LINUX_APP_PLAN.md` say, push the new branch, and move the pin in `apps/desktop/Cargo.toml`. Leave this branch as it is, never rebased or force-pushed, so that the app's older commits still build from their pins.
