# This file is a template — placeholders are substituted by the release workflow.
# The rendered version lives in https://github.com/berg/homebrew-ugpibd
class Ugpibd < Formula
  desc "Userspace daemon for USB-GPIB adapters (Prologix + HiSLIP TCP front-ends)"
  homepage "https://github.com/berg/ugpibd"
  version "__VERSION__"
  license "GPL-3.0-or-later"

  on_macos do
    on_intel do
      url "https://github.com/berg/ugpibd/releases/download/v__VERSION__/ugpibd-v__VERSION__-x86_64-apple-darwin.tar.gz"
      sha256 "__SHA256_X86_MACOS__"
    end
    on_arm do
      url "https://github.com/berg/ugpibd/releases/download/v__VERSION__/ugpibd-v__VERSION__-aarch64-apple-darwin.tar.gz"
      sha256 "__SHA256_ARM_MACOS__"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/berg/ugpibd/releases/download/v__VERSION__/ugpibd-v__VERSION__-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "__SHA256_X86_LINUX__"
    end
    on_arm do
      url "https://github.com/berg/ugpibd/releases/download/v__VERSION__/ugpibd-v__VERSION__-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "__SHA256_ARM_LINUX__"
    end
  end

  def install
    bin.install "ugpibd"
    bin.install "scpi"
  end

  test do
    assert_match "ugpibd", shell_output("#{bin}/ugpibd --help 2>&1")
    assert_match "SCPI", shell_output("#{bin}/scpi --help 2>&1")
  end
end
