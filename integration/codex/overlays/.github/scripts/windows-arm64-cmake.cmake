# BoringSSL's Visual Studio generator emits references to unavailable assembly
# objects on native ARM64 too. btls-sys only disables assembly when HOST != TARGET.
# Use the same supported C implementation for native Windows ARM64 builds.
set(OPENSSL_NO_ASM ON CACHE BOOL "Use portable BoringSSL on Windows ARM64")
set(CMAKE_MSVC_RUNTIME_LIBRARY MultiThreaded CACHE STRING "Match Cargo's static CRT")
