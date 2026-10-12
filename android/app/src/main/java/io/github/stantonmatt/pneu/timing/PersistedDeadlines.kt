package io.github.stantonmatt.pneu.timing

/** Wall time is only a restart hint. Project each saved value once, with a
 * bounded wait, then keep its monotonic deadline until that value changes. */
internal class PersistedDeadlines<K>(private val maximumWait:Long,private val wall:()->Long,private val elapsed:()->Long) {
    private data class Entry(val saved:Long,val deadline:Long)
    private val entries=mutableMapOf<K,Entry>()
    init {require(maximumWait>0)}
    @Synchronized fun at(key:K,saved:Long):Long {
        entries[key]?.takeIf {it.saved==saved}?.let {return it.deadline}
        return add(elapsed(),remaining(saved,wall(),maximumWait)).also {entries[key]=Entry(saved,it)}
    }
    @Synchronized fun remember(key:K,saved:Long,wait:Long) {
        entries[key]=Entry(saved,add(elapsed(),wait.coerceIn(0,maximumWait)))
    }
    @Synchronized fun retain(keys:Set<K>) {entries.keys.retainAll(keys)}
    companion object {
        fun remaining(saved:Long,now:Long,maximum:Long):Long = when {
            saved<=now -> 0
            now<=Long.MAX_VALUE-maximum && saved>now+maximum -> maximum
            else -> saved-now // subtraction cannot overflow after the bound above
        }
        fun add(now:Long,wait:Long)=if(now>Long.MAX_VALUE-1-wait)Long.MAX_VALUE-1 else now+wait
    }
}
