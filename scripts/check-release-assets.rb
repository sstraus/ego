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
  FileUtils.cp_r(File.join(root, 'completions'), dir)
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
    # Catches: release archives deliver the binary but omit generated shell completions.
    %w[ego.bash _ego ego.fish _ego.ps1 ego.elv].each do |completion|
      entry = "ego-#{target}/completions/#{completion}"
      bytes = IO.popen(['tar', '-xOzf', File.join(dir, 'dist', "ego-#{target}.tar.gz"), entry], &:read)
      abort "Archive lost completion #{completion}" unless $?.success? && bytes == File.read(File.join(root, 'completions', completion))
    end
    puts "PASS: #{asset}, archive and relative SHA256 checksums"
  end
  # Catches the tag workflow reading an absent CHANGELOG instead of versioned notes.
  notes_step = workflow.fetch('jobs').fetch('release').fetch('steps').find do |step|
    step['name'] == 'Build checksum file and release notes'
  end.fetch('run')
  notes_directory = File.join(dir, 'docs', 'release-notes')
  FileUtils.mkdir_p(notes_directory)
  notes_path = File.join(notes_directory, '0.4.0.md')
  notes = File.read(File.join(root, 'docs', 'release-notes', '0.4.0.md'))
  File.write(notes_path, notes)
  run_notes = lambda do
    system({'GITHUB_REF_NAME' => 'v0.4.0'}, 'bash', '-c', notes_step,
           chdir: dir, out: File::NULL, err: File::NULL)
  end
  abort 'Workflow could not prepare versioned notes' unless run_notes.call
  abort 'Workflow changed release notes' unless File.read(File.join(dir, 'notes.md')) == notes
  FileUtils.rm(notes_path)
  abort 'Workflow accepted missing release notes' if run_notes.call
  File.write(notes_path, " \n")
  abort 'Workflow accepted empty release notes' if run_notes.call
  puts 'PASS: workflow copies versioned notes and refuses missing or empty notes'

  formula = IO.popen(['bash', File.join(root, 'scripts/homebrew-formula.sh'), 'v0.4.0', 'sstraus/ego', File.join(dir, 'dist')], &:read)
  abort 'Formula generation failed' unless $?.success?
  abort 'Formula version drifted' unless formula.include?('version "0.4.0"') && formula.include?('/releases/download/v0.4.0/')
  rows.reject { |row| row.fetch('target').include?('windows') }.each do |row|
    asset = "ego-#{row.fetch('target')}.tar.gz"
    abort "Formula missing archive #{asset}" unless formula.include?("/#{asset}\"")
    checksum = Digest::SHA256.file(File.join(dir, 'dist', asset)).hexdigest
    abort "Formula checksum drifted for #{asset}" unless formula.include?("sha256 \"#{checksum}\"")
  end
  # Catches: Homebrew downloads completions but never installs them for its shells.
  ['bin.install "ego"', 'bash_completion.install "completions/ego.bash" => "ego"',
   'zsh_completion.install "completions/_ego"', 'fish_completion.install "completions/ego.fish"',
   '(pkgshare/"completions").install "completions/_ego.ps1", "completions/ego.elv"'].each do |install|
    abort "Formula missing installation: #{install}" unless formula.include?(install)
  end
  formula_path = File.join(dir, 'ego.rb')
  File.write(formula_path, formula)
  abort 'Invalid formula Ruby' unless system('ruby', '-c', formula_path)
  puts 'PASS: generated Homebrew formula covers all four Unix assets'
end
