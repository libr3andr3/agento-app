package tech.yaya.agente

/** Where the app goes after sign-in. Kept in one place so notifications,
 *  the update banner and the account screen all agree. */
object Screens {
    /** The home screen once a business exists on this phone. */
    val HOME: Class<*> = DashboardActivity::class.java
    /** After sign-in on a phone with no business yet. */
    val FIRST_RUN: Class<*> = RegistrationActivity::class.java
}
