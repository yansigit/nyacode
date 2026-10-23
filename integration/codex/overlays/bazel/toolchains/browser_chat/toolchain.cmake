# All executable paths are supplied by the Bazel action's native toolchains.
set(CMAKE_MAKE_PROGRAM "$ENV{CODEX_CHAT_NINJA}" CACHE FILEPATH "" FORCE)
# BoringSSL only builds static archives here. Bazel supplies the final native
# libraries/linker through CcInfo; a cross-compiler configure probe must not try
# linking a standalone Windows executable with the Linux host's linker.
set(CMAKE_TRY_COMPILE_TARGET_TYPE STATIC_LIBRARY)
get_filename_component(CMAKE_AR "$ENV{AR}" ABSOLUTE
    BASE_DIR "${CMAKE_CURRENT_LIST_DIR}/../../..")
# Host GNU ranlib cannot rewrite Darwin archives. Keep archive indexing on the
# same declared LLVM toolchain as object compilation and archive creation.
get_filename_component(CMAKE_RANLIB "$ENV{RANLIB}" ABSOLUTE
    BASE_DIR "${CMAKE_CURRENT_LIST_DIR}/../../..")
if("$ENV{TARGET}" MATCHES "apple-darwin$")
  set(CMAKE_SYSTEM_NAME Darwin)
  # CMake requires this tool during compiler detection even for static builds.
  # Cross builds must use the declared LLVM binary instead of probing the host.
  get_filename_component(CMAKE_INSTALL_NAME_TOOL "$ENV{CODEX_CHAT_INSTALL_NAME_TOOL}" ABSOLUTE
      BASE_DIR "${CMAKE_CURRENT_LIST_DIR}/../../..")
elseif("$ENV{TARGET}" MATCHES "windows")
  set(CMAKE_SYSTEM_NAME Windows)
  if("$ENV{TARGET}" MATCHES "windows-msvc$")
    # CMake uses lib.exe argument syntax for MSVC archives; llvm-ar uses the
    # Unix syntax even when it writes COFF objects. Select the declared driver.
    get_filename_component(CMAKE_AR "$ENV{CODEX_CHAT_MSVC_LIB}" ABSOLUTE
        BASE_DIR "${CMAKE_CURRENT_LIST_DIR}/../../..")
    # Match the workspace's Rust and native static CRT policy. CMake's default
    # would append -MD after the exported /MT compiler flag and override it.
    set(CMAKE_MSVC_RUNTIME_LIBRARY MultiThreaded CACHE STRING "" FORCE)
  endif()
  # btls-sys already disables assembly on native Windows hosts. Match that
  # configuration when cross-compiling so it does not require undeclared NASM.
  set(OPENSSL_NO_ASM YES CACHE BOOL "" FORCE)
else()
  set(CMAKE_SYSTEM_NAME Linux)
endif()
string(REGEX REPLACE "-.*" "" CMAKE_SYSTEM_PROCESSOR "$ENV{TARGET}")
