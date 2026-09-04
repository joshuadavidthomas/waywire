package main

import (
	"encoding/json"
	"fmt"
	"os"
	"time"

	"golang.org/x/mod/semver"
)

var (
	buildVersion = "dev"
	buildSource  = "unknown"
)

type installedMetadata struct {
	Release    string            `json:"release"`
	ObservedAt time.Time         `json:"observed_at"`
	Packages   map[string]string `json:"packages"`
}

type versionDocument struct {
	Release   string            `json:"release"`
	Source    string            `json:"source"`
	Installed installedMetadata `json:"installed"`
}

func checkUpgrade(previous, next string) error {
	if !semver.IsValid(previous) || !semver.IsValid(next) {
		return fmt.Errorf("upgrade requires valid release versions, got %q and %q", previous, next)
	}
	if semver.Compare(previous, next) > 0 {
		return fmt.Errorf("newer release %s already owns the installation", previous)
	}
	return nil
}

func loadVersion(path string) (versionDocument, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return versionDocument{}, err
	}
	var installed installedMetadata
	if err := json.Unmarshal(data, &installed); err != nil {
		return versionDocument{}, fmt.Errorf("decode version metadata: %w", err)
	}
	if installed.Release == "" || installed.ObservedAt.IsZero() || installed.Packages == nil {
		return versionDocument{}, fmt.Errorf("version metadata requires release, observed_at, and packages")
	}
	if installed.Release != buildVersion {
		return versionDocument{}, fmt.Errorf("installed release %q does not match binary release %q", installed.Release, buildVersion)
	}
	return versionDocument{Release: buildVersion, Source: buildSource, Installed: installed}, nil
}
