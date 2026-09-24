package main

import (
	_ "embed"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// The delegate skill ships inside the binary so every install channel can
// place it where coding agents look for skills.
//
//go:embed skills/ruddr-delegate/SKILL.md
var delegateSkill string

const delegateSkillName = "ruddr-delegate"

func skillCommand(args []string) error {
	if len(args) == 0 {
		printSkillUsage()
		return errors.New("a skill subcommand is required")
	}
	switch args[0] {
	case "install":
		return skillInstallCommand(args[1:])
	case "show":
		fmt.Print(delegateSkill)
		return nil
	case "help", "--help", "-h":
		printSkillUsage()
		return nil
	default:
		printSkillUsage()
		return fmt.Errorf("unknown skill subcommand %q", args[0])
	}
}

func skillInstallCommand(args []string) error {
	fs := flag.NewFlagSet("skill install", flag.ContinueOnError)
	var dirs stringList
	fs.Var(&dirs, "dir", "skills directory to install into; repeatable (default: ~/.claude/skills, ~/.agents/skills, and ~/.codex/skills when Codex is installed)")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if len(fs.Args()) > 0 {
		return fmt.Errorf("unexpected skill install arguments %q", strings.Join(fs.Args(), " "))
	}
	targets := []string(dirs)
	if len(targets) == 0 {
		var err error
		targets, err = defaultSkillDirectories()
		if err != nil {
			return err
		}
	}
	return installDelegateSkillInto(targets)
}

func installDelegateSkillInto(targets []string) error {
	var failures []string
	for _, dir := range targets {
		path, err := installDelegateSkill(dir)
		if err != nil {
			failures = append(failures, fmt.Sprintf("%s: %v", dir, err))
			continue
		}
		fmt.Printf("installed %s\n", path)
	}
	if len(failures) > 0 {
		return errors.New(strings.Join(failures, "; "))
	}
	return nil
}

// defaultSkillDirectories covers Claude Code, the shared ~/.agents location,
// and Codex when it is installed.
func defaultSkillDirectories() ([]string, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return nil, err
	}
	dirs := []string{
		filepath.Join(home, ".claude", "skills"),
		filepath.Join(home, ".agents", "skills"),
	}
	if info, err := os.Stat(filepath.Join(home, ".codex")); err == nil && info.IsDir() {
		dirs = append(dirs, filepath.Join(home, ".codex", "skills"))
	}
	return dirs, nil
}

// installDelegateSkill writes the skill into <dir>/ruddr-delegate/SKILL.md,
// replacing an earlier copy. It returns the written file path.
func installDelegateSkill(dir string) (string, error) {
	skillDir := filepath.Join(dir, delegateSkillName)
	if err := os.MkdirAll(skillDir, 0o755); err != nil {
		return "", err
	}
	path := filepath.Join(skillDir, "SKILL.md")
	if existing, err := os.ReadFile(path); err == nil && string(existing) == delegateSkill {
		return path, nil
	}
	tmp, err := os.CreateTemp(skillDir, ".SKILL.md-*")
	if err != nil {
		return "", err
	}
	defer os.Remove(tmp.Name())
	if _, err := tmp.WriteString(delegateSkill); err != nil {
		tmp.Close()
		return "", err
	}
	if err := tmp.Chmod(0o644); err != nil {
		tmp.Close()
		return "", err
	}
	if err := tmp.Close(); err != nil {
		return "", err
	}
	if err := os.Rename(tmp.Name(), path); err != nil {
		return "", err
	}
	return path, nil
}

type stringList []string

func (l *stringList) String() string { return strings.Join(*l, ",") }

func (l *stringList) Set(value string) error {
	if value == "" {
		return errors.New("directory must not be empty")
	}
	*l = append(*l, value)
	return nil
}

func printSkillUsage() {
	name := filepath.Base(os.Args[0])
	fmt.Fprintf(os.Stderr, `Usage:
  %[1]s skill install [--dir DIR ...]   copy the ruddr-delegate skill into agent skill directories
  %[1]s skill show                      print the skill

Without --dir the skill is installed into ~/.claude/skills, ~/.agents/skills,
and ~/.codex/skills when ~/.codex exists. ruddr update, the npm postinstall
hook, and scripts/install-local.sh run skill install for you.
`, name)
}
