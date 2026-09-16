{
  symlinkJoin,
  makeWrapper,
  marcel,
}:
# The variant that answers open and save dialogs.
#
# Same shape as the FileManager1 variant: the binary is wrapped to claim the
# portal backend name on every launch, and the activation file lets
# xdg-desktop-portal start Marcel when a dialog is asked for while it is not
# running. The `.portal` file is what makes the frontend consider Marcel at
# all; `portals.conf` (`xdg.portal.config`) is what makes it choose Marcel.
symlinkJoin {
  name = "marcel-file-chooser-portal";
  paths = [ marcel ];
  nativeBuildInputs = [ makeWrapper ];

  postBuild = ''
    wrapProgram "$out/bin/marcel-rs" --set MARCEL_CLAIM_FILE_CHOOSER 1

    # Every activation file underneath must start this wrapper, or a Marcel
    # started by a dialog request runs without the claim and the request
    # times out. This may sit over the FileManager1 variant, so re-point
    # whatever service files are there rather than a fixed list.
    for service in "$out/share/dbus-1/services/"*.service; do
      cp --remove-destination "$(readlink -f "$service")" "$service"
      substituteInPlace "$service" \
        --replace-fail "${marcel}/bin/marcel-rs" "$out/bin/marcel-rs"
    done

    install -Dm644 ${./org.freedesktop.impl.portal.desktop.marcel.service} \
      "$out/share/dbus-1/services/org.freedesktop.impl.portal.desktop.marcel.service"
    substituteInPlace \
      "$out/share/dbus-1/services/org.freedesktop.impl.portal.desktop.marcel.service" \
      --replace-fail @marcel@ "$out"

    install -Dm644 ${./marcel.portal} \
      "$out/share/xdg-desktop-portal/portals/marcel.portal"
  '';

  inherit (marcel) meta version;
  passthru.unconfigured = marcel;
}
