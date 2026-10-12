package io.github.stantonmatt.pneu.features

object ImagePolicy {
    /** EXIF transforms in image coordinates: transpose (5) differs from transverse (7). */
    fun matrix(orientation:Int):FloatArray {
        val m=when(orientation) {
            2->floatArrayOf(-1f,0f,0f,1f);3->floatArrayOf(-1f,0f,0f,-1f)
            4->floatArrayOf(1f,0f,0f,-1f);5->floatArrayOf(0f,1f,1f,0f)
            6->floatArrayOf(0f,-1f,1f,0f);7->floatArrayOf(0f,-1f,-1f,0f)
            8->floatArrayOf(0f,1f,-1f,0f);else->floatArrayOf(1f,0f,0f,1f)
        }
        return floatArrayOf(m[0],m[1],0f,m[2],m[3],0f,0f,0f,1f)
    }
}
