# Libraries latch-host loads at run time rather than links: the window
# (X11 or Wayland, through EGL or Vulkan) and VA-API encoding. NVENC comes
# from the graphics driver, which addDriverRunpath points at.
{
  libGL,
  libva,
  libx11,
  libxcursor,
  libxi,
  libxkbcommon,
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
  vulkan-loader
  wayland
]
