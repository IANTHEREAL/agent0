package logx

import (
	"fmt"
	"log"
	"os"
	"sync/atomic"
)

type Level int32

const (
	Debug Level = iota
	Info
	Warning
	Error
)

var currentLevel atomic.Int32

func init() {
	currentLevel.Store(int32(Info))
	log.SetOutput(os.Stderr)
	log.SetFlags(log.LstdFlags | log.Lmicroseconds)
}

func SetLevel(l Level) { currentLevel.Store(int32(l)) }

func Debugf(format string, args ...any) {
	if Level(currentLevel.Load()) > Debug {
		return
	}
	log.Printf("DEBUG "+format, args...)
}

func Infof(format string, args ...any) {
	if Level(currentLevel.Load()) > Info {
		return
	}
	log.Printf("INFO "+format, args...)
}

func Warningf(format string, args ...any) {
	if Level(currentLevel.Load()) > Warning {
		return
	}
	log.Printf("WARN "+format, args...)
}

func Errorf(format string, args ...any) {
	if Level(currentLevel.Load()) > Error {
		return
	}
	log.Printf("ERROR "+format, args...)
}

func Fatalf(format string, args ...any) {
	log.Printf("FATAL "+format, args...)
	os.Exit(1)
}

func Sprintf(format string, args ...any) string { return fmt.Sprintf(format, args...) }

