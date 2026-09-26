{
  lib,
  libheif,
  dav1d,
  libde265,
  libpng,
  libjpeg,
}:
# libheif as Marcel uses it: a decoder for HEIC and AVIF, nothing else.
#
# nixpkgs builds libheif with every codec it can carry, encoders included:
# x265, rav1e and libaom (with libvmaf behind it) for writing, and a
# gdk-pixbuf loader for other applications. Marcel never encodes, and the
# stock build would add about 50 MiB to its closure, over half of it x265.
# What stays is libde265 for HEVC and dav1d for AV1, a few MiB between them.
#
# libpng and libjpeg are left in because the `bin` output's example tools
# link them, and nixpkgs' postInstall edits the thumbnailer file those tools
# install. Neither is referenced from the `lib` output Marcel links against.
libheif.overrideAttrs (previous: {
  pname = "libheif-decoder";

  buildInputs = [
    dav1d
    libde265
    libpng
    libjpeg
  ];

  cmakeFlags = (previous.cmakeFlags or [ ]) ++ [
    (lib.cmakeBool "WITH_GDK_PIXBUF" false)
    (lib.cmakeBool "WITH_X265" false)
    (lib.cmakeBool "WITH_RAV1E" false)
    (lib.cmakeBool "WITH_AOM_DECODER" false)
    (lib.cmakeBool "WITH_AOM_ENCODER" false)
    (lib.cmakeBool "WITH_DAV1D" true)
    (lib.cmakeBool "WITH_LIBDE265" true)
  ];

  # The stock recipe points the gdk-pixbuf loader's install directory at
  # gdk-pixbuf, which would keep it in the build graph for nothing.
  env = builtins.removeAttrs (previous.env or { }) [
    "PKG_CONFIG_GDK_PIXBUF_2_0_GDK_PIXBUF_MODULEDIR"
  ];
})
