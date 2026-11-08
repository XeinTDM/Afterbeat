# Contributing to Afterbeat

Use GitHub Issues for bug reports and feature requests. For a bug, include the Afterbeat version, Windows version, steps to reproduce, and what you expected. For shortcut, delivery, or timing issues, include the activation mode, output, pace, and target app if you can share it. Strip private paths, app names, and other personal data from attachments. [Security reports](SECURITY.md)

For a pull request, describe the behavior you changed and how you verified it. Keep unrelated changes separate. Discuss broad feature or scheduling changes in an issue first so the expected behavior is clear.

From a Windows x64 checkout, run the project checks in PowerShell 7.

```powershell
.\scripts\verify.ps1
py -3.12 -B -m unittest discover -s scripts/delivery -p 'test_*.py'
```

For changes to input dispatch, global shortcuts, or the Test pad, also run the [native delivery smoke test](docs/build.md#native-delivery-verification) on an interactive Windows test session. For UI changes, check the full and compact layouts and include screenshots when they help explain the change. For dependency changes, review the Windows runtime dependency list against [third-party notices](THIRD-PARTY-NOTICES.txt).

| Area | Location |
| --- | --- |
| Desktop UI | `ui/app.slint`, `src/main.rs` |
| Pacing and run lifecycle | `src/engine.rs`, `src/automation.rs` |
| Windows input and shortcuts | `src/mouse.rs`, `src/hotkeys.rs`, `src/input_filter.rs` |
| Settings and presets | `src/settings.rs` |
| Offscreen previews and native delivery checks | `examples/render_preview.rs`, `scripts/delivery/` |

Please keep discussions focused on the work and treat other contributors with respect. Contributions are made under the project's [MIT license](LICENSE).
