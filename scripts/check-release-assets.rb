#!/usr/bin/env ruby
# Catches platform/asset drift that breaks TUIC-style latest/download URLs.
require 'yaml'
require 'tmpdir'
require 'fileutils'
require 'digest'

root = File.expand_path('..', __dir__)
workflow = YAML.load_file(File.join(root, '.github/workflows/release.yml'))
rows = workflow.fetch('jobs').fetch('build').fetch('strategy').fetch('matrix').fetch('include')
expected = {
  'aarch64-apple-darwin' => 'macos-arm64',
  'x86_64-apple-darwin' => 'macos-x64',
  'x86_64-unknown-linux-gnu' => 'linux-x64',
  'aarch64-unknown-linux-gnu' => 'linux-arm64',
  'x86_64-pc-windows-msvc' => 'windows-x64'
}
abort 'Release matrix changed its public asset contract' unless rows.to_h { |row| [row.fetch('target'), row.fetch('name')] } == expected && rows.size == 5
build_steps = workflow.fetch('jobs').fetch('build').fetch('steps')
abort 'Workflow must use the checked packager' unless build_steps.any? { |step| step['run'] == "scripts/package-release.sh '${{ matrix.target }}' '${{ matrix.name }}'" }
abort 'Linux GLIBC floor is missing' unless build_steps.any? { |step| step['run'] == 'scripts/check-glibc.sh target/${{ matrix.target }}/release/ego 2.35' }

# Exercise the real packager with inert bytes, not a Rust build or installation.
# Windows ZIP creation requires the runner's 7z; its naming is checked above.
scratch = File.expand_path('~/Gits/.tmp')
FileUtils.mkdir_p(scratch)
Dir.mktmpdir('ego-release-check-', scratch) do |dir|
  FileUtils.mkdir_p(File.join(dir, 'scripts'))
  FileUtils.cp(File.join(root, 'scripts/package-release.sh'), File.join(dir, 'scripts'))
  File.write(File.join(dir, 'README.md'), "release fixture\n")
  rows.each do |row|
    target, name = row.values_at('target', 'name')
    windows = target.include?('windows')
    asset = "ego-#{name}#{windows ? '.exe' : ''}"
    if windows
      puts "PASS: matrix asset #{asset} and #{asset}.sha256 (ZIP execution needs Windows 7z)"
      next
    end
    output = File.join(dir, 'target', target, 'release')
    FileUtils.mkdir_p(output)
    File.write(File.join(output, 'ego'), "inert #{target} fixture\n")
    abort 'Packaging failed' unless system('bash', File.join(dir, 'scripts/package-release.sh'), target, name, out: File::NULL)
    [asset, "ego-#{target}.tar.gz"].each do |file|
      path = File.join(dir, 'dist', file)
      expected_sum = "#{Digest::SHA256.file(path).hexdigest}  #{file}\n"
      abort "Bad checksum: #{file}" unless File.read("#{path}.sha256") == expected_sum
    end
    abort 'Archive lost its executable' unless IO.popen(['tar', '-tzf', File.join(dir, 'dist', "ego-#{target}.tar.gz")], &:read).include?("ego-#{target}/ego")
    puts "PASS: #{asset}, archive and relative SHA256 checksums"
  end
  formula = IO.popen(['bash', File.join(root, 'scripts/homebrew-formula.sh'), 'v0.2.0', 'sstraus/ego', File.join(dir, 'dist')], &:read)
  abort 'Formula generation failed' unless $?.success?
  %w[macos-arm64 macos-x64 linux-arm64 linux-x64].each do |name|
    abort "Formula missing #{name}" unless formula.include?("/ego-#{name}\"")
  end
  formula_path = File.join(dir, 'ego.rb')
  File.write(formula_path, formula)
  abort 'Invalid formula Ruby' unless system('ruby', '-c', formula_path)
  puts 'PASS: generated Homebrew formula covers all four Unix assets'
end
