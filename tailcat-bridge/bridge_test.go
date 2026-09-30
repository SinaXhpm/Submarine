package tailcatbridge

import "testing"

func TestRejectsNonTailcatAddress(t *testing.T) {
	if _, err := Start("https://not-a-tailcat.example"); err == nil { t.Fatal("accepted non-Tailcat address") }
}

func TestRejectsInvalidPort(t *testing.T) {
	if _, err := OpenForward(99, 0); err == nil { t.Fatal("accepted port zero") }
}
