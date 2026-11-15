# Libraries latch-host loads at run time rather than links: the window
# (X11 or Wayland, through EGL or Vulkan), VA-API encoding and PipeWire for
# Wayland screen capture. NVENC comes from the graphics driver, which
# addDriverRunpath points at.
{
  libGL,
  libva,
  libx11,
  libxcursor,
  libxi,
  libxkbcommon,
  pipewire,
  vulkan-loader,
  wayland,
}:
[
  libGL
  libva
  libx11
  libxcursor
  libxi
  libxkbcommon
  pipewire
  vulkan-loader
  wayland
]
