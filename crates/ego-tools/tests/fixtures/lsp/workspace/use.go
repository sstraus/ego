package greet

func Both() (string, string) {
	a := "héllo😀"; b := Greeting(a)
	return b, Greeting("world")
}

func Broken() string {
	return Missing()
}
