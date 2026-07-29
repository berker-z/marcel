{
  writeTextFile,
  marcel,
}:
writeTextFile {
  name = "marcel-file-manager1-service";
  destination = "/share/dbus-1/services/org.freedesktop.FileManager1.service";
  text = ''
    [D-BUS Service]
    Name=org.freedesktop.FileManager1
    Exec=${marcel}/bin/marcel
  '';
}
