package demo

// Kotlin has no registered grammar: the generic fallback handles it.
class Greeter(val name: String) {
    fun greet(): String = "hi $name"
}

object Registry

fun topLevel(x: Int): Int = x + 1

const val LIMIT = 10
