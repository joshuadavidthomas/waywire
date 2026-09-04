package main

import "testing"

func TestUpgradeUsesSemanticVersionPrecedence(t *testing.T) {
	for _, test := range []struct {
		previous, next string
		allowed        bool
	}{
		{"v1.0.0-dev.13", "v1.0.0-rc.1", true},
		{"v1.0.0-rc.1", "v1.0.0", true},
		{"v1.0.0", "v1.0.0-rc.1", false},
		{"v1.0.0-beta.2", "v1.0.0-beta.11", true},
		{"v1.0.0-alpha10", "v1.0.0-alpha2", true},
		{"v1.0.0+z", "v1.0.0+a", true},
		{"v1.0.1", "v1.0.0", false},
		{"v1.0.0", "v1.0.0", true},
		{"v01.0.0", "v1.0.0", false},
		{"v1.0.0", "broken", false},
	} {
		t.Run(test.previous+" to "+test.next, func(t *testing.T) {
			err := checkUpgrade(test.previous, test.next)
			if (err == nil) != test.allowed {
				t.Fatalf("allowed=%v, error=%v", test.allowed, err)
			}
		})
	}
}
