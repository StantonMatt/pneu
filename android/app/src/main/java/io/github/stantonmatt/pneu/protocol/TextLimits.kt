package io.github.stantonmatt.pneu.protocol

/** Preserve complete code points and enforce wire byte limits, never UTF-16 counts. */
fun utf8Prefix(text: String, bytes: Int): String {
    val result=StringBuilder();var used=0;var i=0
    while(i<text.length) {
        val point=text.codePointAt(i);i+=Character.charCount(point)
        if(point in 0xD800..0xDFFF || point==0)continue
        val chars=String(Character.toChars(point));val count=chars.toByteArray().size
        if(used+count>bytes)break
        result.append(chars);used+=count
    }
    return result.toString()
}
fun safeFilename(text: String, jpeg: Boolean=false): String {
    val clean=text.replace('/','_').replace('\\','_').replace('\u0000','_')
    val base=if(jpeg)clean.substringBeforeLast('.',clean) else clean
    val name=utf8Prefix(base,if(jpeg)251 else 255).ifEmpty {"Shared item"}
    return if(jpeg)"$name.jpg" else if(name in setOf(".",".."))"Shared item" else name
}
