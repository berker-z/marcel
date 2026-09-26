These are made for Marcel's tests and carry no third-party content. Marcel's
own libheif is decode-only, so they were encoded with the stock nixpkgs
`libheif` (1.23.1) and ImageMagick:

```sh
magick -size 320x240 xc:red -fill blue -draw "rectangle 0,0 159,119" PNG24:photo.png
heif-enc -q 50 -t 160 -o photo.heic photo.png                 # 8-bit, 160×120 thumbnail
heif-enc -q 50 --rotate-cw 90 -o rotated.heic photo.png       # irot box, not rotated pixels
magick -size 64x48 gradient:red-blue photo16.png
heif-enc -A -q 50 -b 10 -o photo.avif photo16.png             # 10-bit AV1
```

The blue quadrant is top-left in `photo.heic`. In `rotated.heic` the pixels
are stored the same way and the container says to turn them a quarter
clockwise, so the blue quadrant belongs top-right.
