# PhotoCraft for Android

This is the native Android tablet build of PhotoCraft. It uses the same Rust editor, renderer,
brush engine, pressure pipeline and document formats as the desktop app. Android's Storage Access
Framework supplies open and save dialogs, so documents can come from Files, Downloads, cloud
providers and removable storage without broad-storage permissions.

## Build and install

Open `apps/photocraft-android/android` in Android Studio, let Gradle sync, then run the `app`
configuration on a device or emulator. The build script compiles `arm64-v8a` for modern Android
tablets and `x86_64` for emulators.

From a terminal with Android SDK and NDK 30 installed:

```sh
cd apps/photocraft-android/android
./gradlew assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

The release is tablet-first but adaptive: it has no fixed screen size or orientation. With
Interface Scale set to Automatic, the Android host enlarges PhotoCraft's compact desktop controls
by 25%; an explicit preference always wins. S Pen pressure reaches the shared egui stylus pipeline
and drives the existing tablet-pressure, pressure-curve and brush dynamics preferences.

## Android behaviour

- **Open** copies selected documents into private app storage before decoding. This gives the Rust
  editor a stable, readable file during the session and avoids asking for broad filesystem access.
- **Save As** uses Android's create-document picker. Later saves in the same app session continue
  to write to that selected document. Restarting the app asks for a destination again, which keeps
  access scoped to a user-selected document.
- **Recovery** snapshots live in the app-private data directory, so recovery still works if Android
  reclaims the activity.

Android currently reports S Pen pressure through winit's shared touch stream. The pen's tilt,
rotation and eraser-end tool type are not exposed by the upstream winit Android backend, so those
three hardware fields remain neutral on Android while pressure works.
