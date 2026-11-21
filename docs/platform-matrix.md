# Desktop backend feature matrix

This matrix describes the current implementation, not a promise that every desktop or application accepts injected input. Linux selects either a native X11 session or the Wayland portal backend; XWayland is not used as a substitute for the X11 backend. See [platform setup and usage](usage.md#install-and-update) for permissions and instructions.

| Capability | Windows | Linux X11 | macOS | Linux Wayland |
| --- | --- | --- | --- | --- |
| Mouse buttons, wheel, key and single-character text output | Supported | Supported; key presses follow the active layout | Supported; key presses follow the active layout | Portal permission required; discrete wheel support depends on the portal, and text depends on the receiving app |
| Tap, timed or held output | Supported | Mouse supports all three; keyboard output requires Tap | Supported | Supported, subject to portal permission |
| Fixed points and point routes | Physical desktop coordinates | Physical desktop coordinates | Logical points | Manually entered logical coordinates on shared displays; the portal must report display bounds |
| Capture pointer coordinates with F8 or a countdown | Supported | Supported | Supported | Unavailable; enter coordinates manually |
| Inside-only zone, stop zone and screen-edge stop | Supported | Supported | Supported | Unavailable; validation blocks these settings because the input session does not provide the live desktop pointer |
| App process filter | Supported | Supported | Supported | Unavailable; validation blocks it because the input session does not identify the receiving app |
| Detect cursor confinement at a saved target | Pauses delivery | Pauses delivery | Pauses delivery | Cannot verify where the cursor landed after an absolute move |
| Activation and emergency shortcuts | Registered by Afterbeat | Registered by Afterbeat; emergency fallback is Ctrl+Shift+F7 | Registered by Afterbeat | Assigned by the desktop's Global Shortcuts portal; emergency binding is required |
| Per-step actions, conditional workflows, pixel and image detection, and Smart Targets | Supported | Unavailable | Unavailable | Unavailable |

On Wayland, **Enable input** requires a desktop portal backend with Global Shortcuts and Remote Desktop support. **Enable with displays** additionally needs usable logical display bounds for fixed points and routes. Follow-cursor output does not require display sharing. A revoked portal session or missing emergency shortcut stops delivery. Keyboard output pauses while Afterbeat is focused; for keyboard output with While held, use an unmodified activation shortcut. See the [Wayland setup](usage.md#wayland) for details.

The Wayland limitations above describe the data exposed to Afterbeat's current portal sessions, not a claim that every possible compositor-specific protocol lacks those capabilities. Supporting them would require a different source of desktop-wide state and appropriate permission and safety handling.

## Maintenance

Update this matrix when changing backend validation, permissions, target coordinates, input delivery, shortcut handling, or platform-specific route actions. Check [Wayland validation](../src/wayland.rs), [automation guards](../src/automation.rs), the platform input implementations in [src/mouse](../src/mouse), and [platform usage](usage.md) together. Keep the matrix scoped to implemented behavior and note desktop or permission dependencies rather than treating platform builds alone as proof of support.
